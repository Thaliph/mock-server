//! The latency draw and the mode headers (spec §8, §7.7).

use crate::Mode;
use crate::discovery::histogram::Histogram;

/// No delay is longer than this, in milliseconds.
pub const MAX_DELAY_MS: u64 = 300_000;
/// The latency factor in hundredths: 100.00.
pub const MAX_FACTOR: u32 = 10_000;

/// The random numbers of a draw, injected by the caller.
pub trait Random {
    /// A uniform number in `0..bound` (`bound` is at least 1).
    fn below(&mut self, bound: u64) -> u64;
}

/// A latency in whole milliseconds drawn from the histogram, then scaled by `factor`.
pub fn draw(histogram: &Histogram, factor: u32, rng: &mut impl Random) -> u64 {
    let _ = (histogram, factor, rng);
    u64::MAX
}

/// `(d × f + 50) / 100` ms, capped at 300 s.
pub fn scale(d: u64, factor: u32) -> u64 {
    let _ = (d, factor);
    u64::MAX
}

/// A latency factor written as a decimal 0–100, in hundredths rounded to the nearest, halves up.
pub fn parse_factor(text: &str) -> Option<u32> {
    let _ = text;
    None
}

/// The shortest decimal that reads back to `hundredths`.
pub fn format_factor(hundredths: u32) -> String {
    let _ = hundredths;
    String::new()
}

/// The `Mimic-Latency-Factor` of an answer: exactly one valid value.
pub fn parse_factor_header(values: &[&str]) -> Option<u32> {
    let _ = values;
    None
}

/// The `Mimic-Mode` of an answer: exactly one value, `discovery` or `mock`.
pub fn parse_mode_header(values: &[&str]) -> Option<Mode> {
    let _ = values;
    None
}

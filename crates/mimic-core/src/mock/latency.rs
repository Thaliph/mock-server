//! The latency draw and the mode headers (spec §8, §7.7).

use crate::Mode;
use crate::discovery::histogram::{Histogram, OVERFLOW};

/// No delay is longer than this, in milliseconds.
pub const MAX_DELAY_MS: u64 = 300_000;
/// The latency factor in hundredths: 100.00.
pub const MAX_FACTOR: u32 = 10_000;
/// What the overflow bucket draws, in milliseconds (2^17).
const OVERFLOW_MS: u64 = 1 << 17;
/// A written factor is at most this many bytes.
const MAX_FACTOR_TEXT: usize = 32;

/// The random numbers of a draw, injected by the caller: the core has no random source.
pub trait Random {
    /// A uniform number in `0..bound` (`bound` is at least 1).
    fn below(&mut self, bound: u64) -> u64;
}

/// A latency in whole milliseconds drawn from the histogram, then scaled by `factor`
/// (hundredths): a bucket with probability count / total, then a whole number of milliseconds
/// uniformly within it. An empty histogram gives 0.
pub fn draw(histogram: &Histogram, factor: u32, rng: &mut impl Random) -> u64 {
    let total = histogram.count();
    if total == 0 {
        return 0;
    }
    let mut pick = rng.below(total);
    let mut chosen = OVERFLOW;
    for (bucket, count) in histogram.buckets() {
        if pick < *count {
            chosen = *bucket;
            break;
        }
        pick -= *count;
    }
    scale(in_bucket(chosen, rng), factor)
}

/// `(d × f + 50) / 100` ms, rounded to the nearest with halves up, capped at 300 s.
pub fn scale(d: u64, factor: u32) -> u64 {
    let scaled = (u128::from(d) * u128::from(factor) + 50) / 100;
    u64::try_from(scaled).map_or(MAX_DELAY_MS, |ms| ms.min(MAX_DELAY_MS))
}

/// A whole number of milliseconds in bucket `i`: the integers in (2^((i-1)/4), 2^(i/4)],
/// bucket 0 holding 0 and 1, the overflow bucket 131 072. A bucket between two integers (the
/// first ones are) holds none: it gives the integer above its bound.
fn in_bucket(bucket: u8, rng: &mut impl Random) -> u64 {
    if bucket >= OVERFLOW {
        return OVERFLOW_MS;
    }
    if bucket == 0 {
        return rng.below(2);
    }
    let low = floor_root(bucket - 1);
    let high = floor_root(bucket);
    if high <= low {
        return high + 1;
    }
    low + 1 + rng.below(high - low)
}

/// The largest `m` with `m^4 <= 2^power` (`power` at most 68).
fn floor_root(power: u8) -> u64 {
    let limit = 1u128 << power;
    let (mut low, mut high) = (1u64, OVERFLOW_MS);
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if u128::from(mid).pow(4) <= limit {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    low
}

/// A latency factor written as a decimal 0–100 (digits, then optionally a dot and digits), in
/// hundredths, rounded to the nearest with halves up. `None` for anything else.
pub fn parse_factor(text: &str) -> Option<u32> {
    if text.is_empty() || text.len() > MAX_FACTOR_TEXT {
        return None;
    }
    let (whole, fraction) = match text.split_once('.') {
        Some((whole, fraction)) if !fraction.is_empty() => (whole, fraction),
        Some(_) => return None,
        None => (text, ""),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(whole) || (!fraction.is_empty() && !digits(fraction)) {
        return None;
    }
    let whole = whole.trim_start_matches('0');
    if whole.len() > 3 {
        return None;
    }
    let whole: u32 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    let mut hundredths = whole.checked_mul(100)?;
    let mut rest = fraction.bytes().map(|b| u32::from(b - b'0'));
    hundredths += rest.next().unwrap_or(0) * 10;
    hundredths += rest.next().unwrap_or(0);
    hundredths += u32::from(rest.next().unwrap_or(0) >= 5);
    (hundredths <= MAX_FACTOR).then_some(hundredths)
}

/// The shortest decimal that reads back to `hundredths`: `100` is `1`, `50` is `0.5`.
pub fn format_factor(hundredths: u32) -> String {
    let (whole, fraction) = (hundredths / 100, hundredths % 100);
    match fraction {
        0 => whole.to_string(),
        f if f % 10 == 0 => format!("{whole}.{}", f / 10),
        f => format!("{whole}.{f:02}"),
    }
}

/// A factor as a JSON number: `1`, `0.5`, `0.05` (the shortest form, as [`format_factor`]).
pub fn factor_number(hundredths: u32) -> serde_json::Number {
    if hundredths.is_multiple_of(100) {
        serde_json::Number::from(hundredths / 100)
    } else {
        // Division of two integers is correctly rounded: it prints as the decimal written.
        serde_json::Number::from_f64(f64::from(hundredths) / 100.0).unwrap_or_else(|| 0.into())
    }
}

/// The `Mimic-Latency-Factor` of an answer, in hundredths: exactly one valid value.
pub fn parse_factor_header(values: &[&str]) -> Option<u32> {
    match values {
        [value] => parse_factor(value),
        _ => None,
    }
}

/// The `Mimic-Mode` of an answer: exactly one value, `discovery` or `mock`.
pub fn parse_mode_header(values: &[&str]) -> Option<Mode> {
    match values {
        [value] => value.parse().ok(),
        _ => None,
    }
}

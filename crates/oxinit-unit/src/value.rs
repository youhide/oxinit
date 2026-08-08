//! Duration and size string parsers.
//!
//! Both reject rather than guess. A bare number is an error, not an assumed
//! unit; a fractional value is an error, not a rounded one. See
//! `docs/UNIT_FORMAT.md`.

use std::fmt;
use std::time::Duration;

use serde::de::{self, Deserializer};
use serde::Deserialize;

use crate::error::ValueError;

/// A duration written as one or more `<number><suffix>` pairs: `5s`, `2min`,
/// `1h30min`, `100ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurationValue(pub Duration);

/// A byte count written with an optional binary suffix: `256M`, `1G`, `65536`,
/// or the literal `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeValue {
    Bytes(u64),
    Max,
}

const MICROS_PER: &[(&str, u64)] = &[
    // Longest first: `msec` must win over `ms`, `sec` over `s`.
    ("usec", 1),
    ("us", 1),
    ("msec", 1_000),
    ("ms", 1_000),
    ("seconds", 1_000_000),
    ("sec", 1_000_000),
    ("s", 1_000_000),
    ("minutes", 60 * 1_000_000),
    ("min", 60 * 1_000_000),
    ("hours", 3_600 * 1_000_000),
    ("h", 3_600 * 1_000_000),
    ("days", 86_400 * 1_000_000),
    ("d", 86_400 * 1_000_000),
];

/// Parse a duration string into microseconds, then a [`Duration`].
///
/// Pairs are summed, so `1min 30s` is 90 seconds and `30s 30s` is one minute.
/// Order is not enforced and a repeated unit is summed rather than rejected.
pub fn parse_duration(input: &str) -> Result<Duration, ValueError> {
    let mut rest = input.trim();
    if rest.is_empty() {
        return Err(ValueError::EmptyDuration);
    }

    let mut micros: u64 = 0;
    while !rest.is_empty() {
        let digits = take_while(rest, |c| c.is_ascii_digit());
        if digits.is_empty() {
            // Covers "-5s", "1.5s", and a stray suffix with no number.
            return Err(ValueError::DurationSyntax {
                input: input.to_owned(),
            });
        }
        rest = advance(rest, digits.len());

        // No whitespace is allowed between the number and its suffix, so this
        // deliberately does not trim first: "5 s" fails here.
        let suffix = take_while(rest, |c| c.is_ascii_alphabetic());
        if suffix.is_empty() {
            return Err(ValueError::DurationSuffix {
                input: input.to_owned(),
            });
        }
        rest = advance(rest, suffix.len()).trim_start();

        let multiplier = MICROS_PER
            .iter()
            .find(|(name, _)| *name == suffix)
            .map(|(_, m)| *m)
            .ok_or_else(|| ValueError::DurationUnit {
                unit: suffix.to_owned(),
            })?;

        let value: u64 = digits.parse().map_err(|_| ValueError::DurationOverflow)?;

        micros = value
            .checked_mul(multiplier)
            .and_then(|scaled| micros.checked_add(scaled))
            .ok_or(ValueError::DurationOverflow)?;
    }

    Ok(Duration::from_micros(micros))
}

/// Parse a size string into bytes. Multiples are binary: `1K` is 1024.
pub fn parse_size(input: &str) -> Result<SizeValue, ValueError> {
    let text = input.trim();
    if text == "max" {
        return Ok(SizeValue::Max);
    }
    if text.is_empty() {
        return Err(ValueError::EmptySize);
    }

    let digits = take_while(text, |c| c.is_ascii_digit());
    if digits.is_empty() {
        return Err(ValueError::SizeSyntax {
            input: input.to_owned(),
        });
    }

    // Case-sensitive on purpose: "256m" is an error rather than a guess about
    // whether it meant mega or milli.
    let multiplier = match advance(text, digits.len()) {
        "" => 1,
        "K" => 1 << 10,
        "M" => 1 << 20,
        "G" => 1 << 30,
        "T" => 1 << 40,
        other => {
            return Err(ValueError::SizeUnit {
                unit: other.to_owned(),
            })
        }
    };

    let value: u64 = digits.parse().map_err(|_| ValueError::SizeOverflow)?;
    value
        .checked_mul(multiplier)
        .map(SizeValue::Bytes)
        .ok_or(ValueError::SizeOverflow)
}

/// Leading run of characters matching `pred`.
fn take_while(text: &str, pred: impl Fn(char) -> bool) -> &str {
    let end = text
        .char_indices()
        .find(|(_, c)| !pred(*c))
        .map_or(text.len(), |(i, _)| i);
    text.get(..end).unwrap_or("")
}

/// `text` past its first `n` bytes.
fn advance(text: &str, n: usize) -> &str {
    text.get(n..).unwrap_or("")
}

impl fmt::Display for SizeValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // The value written to a cgroup file, verbatim.
            SizeValue::Bytes(n) => write!(f, "{n}"),
            SizeValue::Max => f.write_str("max"),
        }
    }
}

impl<'de> Deserialize<'de> for DurationValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        parse_duration(&raw)
            .map(DurationValue)
            .map_err(de::Error::custom)
    }
}

impl<'de> Deserialize<'de> for SizeValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        parse_size(&raw).map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn micros(input: &str) -> u64 {
        parse_duration(input).unwrap().as_micros() as u64
    }

    #[test]
    fn duration_accepts_each_suffix() {
        assert_eq!(micros("1us"), 1);
        assert_eq!(micros("1usec"), 1);
        assert_eq!(micros("1ms"), 1_000);
        assert_eq!(micros("1msec"), 1_000);
        assert_eq!(micros("1s"), 1_000_000);
        assert_eq!(micros("1sec"), 1_000_000);
        assert_eq!(micros("1seconds"), 1_000_000);
        assert_eq!(micros("1min"), 60_000_000);
        assert_eq!(micros("1minutes"), 60_000_000);
        assert_eq!(micros("1h"), 3_600_000_000);
        assert_eq!(micros("1hours"), 3_600_000_000);
        assert_eq!(micros("1d"), 86_400_000_000);
        assert_eq!(micros("1days"), 86_400_000_000);
    }

    #[test]
    fn duration_sums_pairs() {
        assert_eq!(micros("1min 30s"), 90_000_000);
        assert_eq!(micros("1h30min"), 5_400_000_000);
        // A repeated unit is summed, not rejected.
        assert_eq!(micros("30s 30s"), 60_000_000);
    }

    #[test]
    fn duration_rejects_bare_number() {
        // The spec's headline case: "5" must not be guessed at as seconds.
        assert!(parse_duration("5").is_err());
    }

    #[test]
    fn duration_rejects_malformed() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("5 s").is_err(), "space before suffix");
        assert!(parse_duration("-5s").is_err(), "negative");
        assert!(parse_duration("1.5s").is_err(), "fractional");
        assert!(parse_duration("5x").is_err(), "unknown unit");
        assert!(parse_duration("s").is_err(), "suffix with no number");
    }

    #[test]
    fn duration_rejects_overflow() {
        assert!(parse_duration("18446744073709551615d").is_err());
    }

    #[test]
    fn size_multiples_are_binary() {
        assert_eq!(parse_size("1K").unwrap(), SizeValue::Bytes(1024));
        assert_eq!(
            parse_size("256M").unwrap(),
            SizeValue::Bytes(256 * 1024 * 1024)
        );
        assert_eq!(
            parse_size("1G").unwrap(),
            SizeValue::Bytes(1024 * 1024 * 1024)
        );
        assert_eq!(parse_size("65536").unwrap(), SizeValue::Bytes(65536));
        assert_eq!(parse_size("max").unwrap(), SizeValue::Max);
    }

    #[test]
    fn size_rejects_malformed() {
        assert!(parse_size("256MB").is_err(), "two-letter suffix");
        assert!(parse_size("256 M").is_err(), "space before suffix");
        assert!(parse_size("1.5G").is_err(), "fractional");
        assert!(parse_size("256m").is_err(), "lowercase suffix");
        assert!(parse_size("").is_err());
    }

    #[test]
    fn size_rejects_overflow() {
        assert!(parse_size("18446744073709551615T").is_err());
    }

    #[test]
    fn size_displays_as_cgroup_writes_it() {
        assert_eq!(SizeValue::Bytes(1024).to_string(), "1024");
        assert_eq!(SizeValue::Max.to_string(), "max");
    }
}

//! Go-style duration strings ("30s", "5m", "1h30m", "250ms").
//!
//! The command-line flags and deployment manifests use Go's `time.Duration`
//! syntax, so the same strings are accepted here. Negative durations are
//! clamped to zero: every flag that takes a duration treats "zero or less" as
//! "use the default" or "disabled", so the sign carries no other meaning.

use std::fmt::Write as _;
use std::time::Duration;

/// Fractional digits considered per component; more cannot change the result.
const MAX_FRACTION_DIGITS: usize = 19;

/// Error returned for a malformed duration string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid duration {0:?}")]
pub struct ParseDurationError(String);

/// Parses a duration in Go's `time.ParseDuration` syntax: an optional sign
/// followed by one or more decimal numbers, each with a unit suffix (`ns`,
/// `us`, `µs`, `μs`, `ms`, `s`, `m`, `h`). A bare `0` is also accepted.
pub fn parse(input: &str) -> Result<Duration, ParseDurationError> {
    let err = || ParseDurationError(input.to_string());
    let mut s = input;
    let mut negative = false;
    if let Some(rest) = s.strip_prefix('-') {
        negative = true;
        s = rest;
    } else if let Some(rest) = s.strip_prefix('+') {
        s = rest;
    }
    if s == "0" {
        return Ok(Duration::ZERO);
    }
    if s.is_empty() {
        return Err(err());
    }

    let mut total_nanos: u128 = 0;
    while !s.is_empty() {
        // Integer part.
        let int_len = s.bytes().take_while(u8::is_ascii_digit).count();
        let (int_part, rest) = s.split_at(int_len);
        s = rest;
        // Fractional part.
        let mut frac_part = "";
        if let Some(rest) = s.strip_prefix('.') {
            let frac_len = rest.bytes().take_while(u8::is_ascii_digit).count();
            frac_part = &rest[..frac_len];
            s = &rest[frac_len..];
        }
        if int_part.is_empty() && frac_part.is_empty() {
            return Err(err());
        }
        // Unit.
        let unit_len = s
            .char_indices()
            .find(|&(_, c)| c == '.' || c.is_ascii_digit())
            .map_or(s.len(), |(i, _)| i);
        let (unit, rest) = s.split_at(unit_len);
        s = rest;
        let unit_nanos: u128 = match unit {
            "ns" => 1,
            "us" | "µs" | "μs" => 1_000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "m" => 60 * 1_000_000_000,
            "h" => 3600 * 1_000_000_000,
            _ => return Err(err()),
        };

        let int_value: u128 = if int_part.is_empty() {
            0
        } else {
            int_part.parse().map_err(|_| err())?
        };
        let mut nanos = int_value.checked_mul(unit_nanos).ok_or_else(err)?;
        if !frac_part.is_empty() {
            // Digits beyond nanosecond precision cannot matter: even an hour
            // is only 3.6e12ns. Capping keeps the arithmetic far from overflow.
            let digits = &frac_part[..frac_part.len().min(MAX_FRACTION_DIGITS)];
            let frac_value: u128 = digits.parse().map_err(|_| err())?;
            let scale = 10u128.pow(digits.len() as u32);
            nanos = nanos
                .checked_add(frac_value * unit_nanos / scale)
                .ok_or_else(err)?;
        }
        total_nanos = total_nanos.checked_add(nanos).ok_or_else(err)?;
    }

    if negative {
        return Ok(Duration::ZERO);
    }
    let secs = u64::try_from(total_nanos / 1_000_000_000).map_err(|_| err())?;
    Ok(Duration::new(secs, (total_nanos % 1_000_000_000) as u32))
}

/// Formats a duration the way Go's `time.Duration.String` does, for logs.
pub fn format(d: Duration) -> String {
    let nanos = d.as_nanos();
    if nanos == 0 {
        return "0s".to_string();
    }
    if nanos < 1_000 {
        return format!("{nanos}ns");
    }
    if nanos < 1_000_000 {
        return format!("{}µs", trim_fraction(nanos, 1_000));
    }
    if nanos < 1_000_000_000 {
        return format!("{}ms", trim_fraction(nanos, 1_000_000));
    }
    let total_secs = d.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let secs_nanos = nanos % 60_000_000_000;
    let seconds = trim_fraction(secs_nanos, 1_000_000_000);
    let mut out = String::new();
    if hours > 0 {
        let _ = write!(out, "{hours}h");
    }
    if hours > 0 || minutes > 0 {
        let _ = write!(out, "{minutes}m");
    }
    let _ = write!(out, "{seconds}s");
    out
}

fn trim_fraction(value: u128, unit: u128) -> String {
    let whole = value / unit;
    let frac = value % unit;
    if frac == 0 {
        return whole.to_string();
    }
    let width = unit.ilog10() as usize;
    let frac = format!("{frac:0width$}");
    format!("{whole}.{}", frac.trim_end_matches('0'))
}

/// A clap value parser for Go-style durations.
pub fn clap_parser(s: &str) -> Result<Duration, String> {
    parse(s).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_go_durations() {
        let cases = [
            ("0", Duration::ZERO),
            ("0s", Duration::ZERO),
            ("30s", Duration::from_secs(30)),
            ("5m", Duration::from_secs(300)),
            ("1h", Duration::from_secs(3600)),
            ("1h30m", Duration::from_secs(5400)),
            ("250ms", Duration::from_millis(250)),
            ("1.5h", Duration::from_secs(5400)),
            (".5s", Duration::from_millis(500)),
            ("1us", Duration::from_micros(1)),
            ("1µs", Duration::from_micros(1)),
            ("10ns", Duration::from_nanos(10)),
            ("+2s", Duration::from_secs(2)),
            ("-5s", Duration::ZERO),
            ("2m3.5s", Duration::from_millis(123_500)),
            (
                "1.000000000000000000000000000001h",
                Duration::from_secs(3600),
            ),
            ("0.5h", Duration::from_secs(1800)),
        ];
        for (input, want) in cases {
            assert_eq!(parse(input), Ok(want), "{input}");
        }
    }

    #[test]
    fn rejects_malformed_durations() {
        for input in ["", "5", "s", "1x", "1.s.", "-", "1h-1m", "abc", "1 s"] {
            assert!(parse(input).is_err(), "{input:?} should not parse");
        }
    }

    #[test]
    fn formats_like_go() {
        assert_eq!(format(Duration::ZERO), "0s");
        assert_eq!(format(Duration::from_millis(100)), "100ms");
        assert_eq!(format(Duration::from_millis(1500)), "1.5s");
        assert_eq!(format(Duration::from_secs(30)), "30s");
        assert_eq!(format(Duration::from_secs(600)), "10m0s");
        assert_eq!(format(Duration::from_secs(5400)), "1h30m0s");
        assert_eq!(format(Duration::from_nanos(15)), "15ns");
    }
}

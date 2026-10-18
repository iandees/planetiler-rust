//! Port of `util/Format.java`: utilities for formatting numbers, sizes, and durations as strings for logs.
//!
//! Java's `Format` is locale-aware; this port always formats using US conventions (`,` grouping separator and `.`
//! decimal separator), equivalent to `Format.forLocale(Locale.US)`.

use std::time::Duration;

/// A primitive number that can be formatted by the functions in this module (Java's `Number`).
pub trait Number: Copy {
    fn as_f64(self) -> f64;
    /// Equivalent of Java's `Number.longValue()` (truncates towards zero).
    fn as_i64(self) -> i64;
}

macro_rules! impl_number {
    ($($t:ty),*) => {
        $(impl Number for $t {
            #[inline]
            fn as_f64(self) -> f64 {
                self as f64
            }
            #[inline]
            fn as_i64(self) -> i64 {
                self as i64
            }
        })*
    };
}
impl_number!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize, f32, f64);

const STORAGE_SUFFIXES: [(i64, &str); 5] =
    [(1_000, "k"), (1_000_000, "M"), (1_000_000_000, "G"), (1_000_000_000_000, "T"), (1_000_000_000_000_000, "P")];

const NUMERIC_SUFFIXES: [(i64, &str); 5] =
    [(1_000, "k"), (1_000_000, "M"), (1_000_000_000, "B"), (1_000_000_000_000, "T"), (1_000_000_000_000_000, "Q")];

/// Pads `s` with spaces on the right until it is at least `size` characters long.
pub fn pad_right(s: &str, size: usize) -> String {
    format!("{s:<size$}")
}

/// Pads `s` with spaces on the left until it is at least `size` characters long.
pub fn pad_left(s: &str, size: usize) -> String {
    format!("{s:>size$}")
}

/// Returns `items` rounded to 5 decimals and joined with a comma.
pub fn join_coordinates(items: &[f64]) -> String {
    items.iter().map(|d| format_fraction(*d, 5, false)).collect::<Vec<_>>().join(",")
}

/// Returns Java/Rust source code that can re-create `string`: `null` if `None`, otherwise a quoted, escaped string.
pub fn quote(string: Option<&str>) -> String {
    match string {
        None => "null".to_string(),
        Some(s) => {
            let mut out = String::with_capacity(s.len() + 2);
            out.push('"');
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    '\u{8}' => out.push_str("\\b"),
                    '\u{c}' => out.push_str("\\f"),
                    c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                        let mut buf = [0u16; 2];
                        for unit in c.encode_utf16(&mut buf) {
                            out.push_str(&format!("\\u{unit:04X}"));
                        }
                    }
                    c => out.push(c),
                }
            }
            out.push('"');
            out
        }
    }
}

/// Returns an openstreetmap.org map link for a lat/lon.
pub fn osm_debug_url(zoom: u32, lon: f64, lat: f64) -> String {
    format!(
        "https://www.openstreetmap.org/#map={zoom}/{}/{}",
        format_fraction(lat, 5, false),
        format_fraction(lon, 5, false)
    )
}

/// Returns a number of bytes formatted like "123" "1.2k" "240M", etc.
pub fn storage(num: impl Number, pad: bool) -> String {
    format_with_suffix(num, pad, &STORAGE_SUFFIXES)
}

/// Returns a number formatted like "123" "1.2k" "2.5B", etc.
pub fn numeric(num: impl Number, pad: bool) -> String {
    format_with_suffix(num, pad, &NUMERIC_SUFFIXES)
}

fn format_with_suffix(num: impl Number, pad: bool, suffixes: &[(i64, &str)]) -> String {
    let value = num.as_i64();
    let double_value = num.as_f64();
    let width = if pad { 4 } else { 0 };
    if value < 0 {
        return pad_left("-", width);
    } else if double_value > 0.0 && double_value < 1.0 {
        return pad_left("<1", width);
    } else if value < 1000 {
        return pad_left(&value.to_string(), width);
    }
    let (divide_by, suffix) = suffixes.iter().rev().find(|(k, _)| *k <= value).copied().unwrap_or(suffixes[0]);
    let truncated = value / (divide_by / 10);
    let has_decimal = truncated < 100 && truncated % 10 != 0;
    let result = if has_decimal {
        format!("{}{suffix}", decimal(truncated as f64 / 10.0))
    } else {
        format!("{}{suffix}", truncated / 10)
    };
    pad_left(&result, width)
}

/// Returns 0.0-1.0 as "0%" - "100%" with no decimal points.
pub fn percent(value: f64) -> String {
    if value.is_nan() {
        return "NaN%".to_string();
    }
    format!("{}%", format_fraction(value * 100.0, 0, true))
}

/// Returns a number formatted with up to 1 decimal point and grouping separators.
pub fn decimal(value: f64) -> String {
    format_fraction(value, 1, true)
}

/// Returns a number formatted with 0 decimal points and grouping separators.
pub fn integer(value: impl Number) -> String {
    format_fraction(value.as_f64(), 0, true)
}

/// Returns a duration formatted as fractional seconds with 1 decimal point.
pub fn seconds(d: Duration) -> String {
    let secs = d.as_secs_f64();
    format!("{}s", decimal(if secs < 1.0 { secs } else { secs.round() }))
}

/// Returns a duration formatted like "1h2m" or "2m3s" (or "0.5s" when less than a second).
pub fn duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 1.0 {
        return format!("{}s", decimal(secs));
    }
    let total = secs.round() as u64;
    let (h, m, s) = (total / 3600, (total / 60) % 60, total % 60);
    let mut out = String::new();
    if h > 0 {
        out.push_str(&format!("{h}h"));
    }
    if m > 0 {
        out.push_str(&format!("{m}m"));
    }
    if s > 0 {
        out.push_str(&format!("{s}s"));
    }
    out
}

/// Formats `value` with at most `max_fraction_digits` digits after the decimal point (trailing zeros stripped) like
/// Java's `NumberFormat`, optionally with `,` grouping separators.
pub fn format_fraction(value: f64, max_fraction_digits: usize, grouping: bool) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "∞".to_string() } else { "-∞".to_string() };
    }
    let formatted = format!("{:.*}", max_fraction_digits, value.abs());
    let (int_part, frac_part) = match formatted.split_once('.') {
        Some((i, f)) => (i, f.trim_end_matches('0')),
        None => (formatted.as_str(), ""),
    };
    let negative = value < 0.0 && (int_part.bytes().any(|b| b != b'0') || !frac_part.is_empty());
    let mut out = String::with_capacity(formatted.len() + 8);
    if negative {
        out.push('-');
    }
    if grouping {
        let len = int_part.len();
        for (i, c) in int_part.chars().enumerate() {
            if i > 0 && (len - i) % 3 == 0 {
                out.push(',');
            }
            out.push(c);
        }
    } else {
        out.push_str(int_part);
    }
    if !frac_part.is_empty() {
        out.push('.');
        out.push_str(frac_part);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_numeric() {
        for (num, expected) in [
            (1.5, "1"),
            (999.0, "999"),
            (1000.0, "1k"),
            (9999.0, "9.9k"),
            (10001.0, "10k"),
            (99999.0, "99k"),
            (999999.0, "999k"),
            (9999999.0, "9.9M"),
            (-9999999.0, "-"),
            (5.5e12, "5.5T"),
        ] {
            assert_eq!(expected, numeric(num, false), "{num}");
        }
        assert_eq!("  1k", numeric(1000, true));
        assert_eq!("  <1", numeric(0.5, true));
    }

    #[test]
    fn format_storage() {
        for (num, expected) in [(999.0, "999"), (1000.0, "1k"), (9999.0, "9.9k"), (5.5e9, "5.5G")] {
            assert_eq!(expected, storage(num, false), "{num}");
        }
    }

    #[test]
    fn format_percent() {
        assert_eq!("0%", percent(0.0));
        assert_eq!("100%", percent(1.0));
        assert_eq!("11%", percent(0.11111));
    }

    #[test]
    fn pad() {
        for (input, size, out) in
            [("a", 0, "a"), ("a", 1, "a"), ("a", 2, " a"), ("a", 3, "  a"), ("ab", 3, " ab"), ("abc", 3, "abc")]
        {
            assert_eq!(out, pad_left(input, size));
        }
        assert_eq!("a  ", pad_right("a", 3));
    }

    #[test]
    fn format_decimal() {
        assert_eq!("0", decimal(0.0));
        assert_eq!("0.1", decimal(0.1));
        assert_eq!("0.1", decimal(0.11));
        assert_eq!("1,111.1", decimal(1111.11));
        assert_eq!("-1.5", decimal(-1.5));
        assert_eq!("0", decimal(-0.01));
        assert_eq!("1,234,567", integer(1234567));
    }

    #[test]
    fn format_duration() {
        for (secs, out) in [
            (0.0, "0s"),
            (0.1, "0.1s"),
            (0.999, "1s"),
            (1.1, "1s"),
            (59.0, "59s"),
            (60.0, "1m"),
            (61.1, "1m1s"),
            (3599.0, "59m59s"),
            (3600.0, "1h"),
            (3601.0, "1h1s"),
        ] {
            assert_eq!(out, duration(Duration::from_secs_f64(secs)), "{secs}");
        }
    }

    #[test]
    fn osm_url() {
        assert_eq!("https://www.openstreetmap.org/#map=1/2.3/1.2", osm_debug_url(1, 1.2, 2.3));
    }

    #[test]
    fn join_coords() {
        assert_eq!("1.12346,2,-3", join_coordinates(&[1.123456, 2.0, -3.0]));
    }

    #[test]
    fn quoting() {
        assert_eq!("null", quote(None));
        assert_eq!("\"a\\\"b\\n\"", quote(Some("a\"b\n")));
    }
}

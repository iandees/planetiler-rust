//! Port of `util/Parse.java`: utilities to parse values from strings.
//!
//! Functions that take a [`Value`] mirror the Java methods that accept an `Object`: numbers are converted directly,
//! anything else is parsed from its string form. `_str` variants accept a plain string.

use std::sync::LazyLock;

use regex::Regex;

use crate::util::imposm3_parsers;
use crate::value::{Tags, Value};

static INT_SUBSTRING_PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(-?\d+)(\D|$)").unwrap());
static TO_ROUND_INT_SUBSTRING_PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(-?[\d.]+)(\D|$)").unwrap());
// See https://wiki.openstreetmap.org/wiki/Map_features/Units
static DISTANCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)(?<value>-?[\d.]+)\s*((?<mi>mi)|(?<m>m|$)|(?<km>km|kilom)|(?<ft>ft|')|(?<in>in|")|(?<nmi>nmi|international nautical mile|nautical))"#,
    )
    .unwrap()
});
static NUMBER_WITH_UNIT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?<value>-?[\d.]+)\s*(?<unit>[^.\d]*)").unwrap());

/// Equivalent of Java's `Double.parseDouble`: trims whitespace, allows a trailing `d`/`f` type suffix, and accepts
/// `NaN`/`Infinity` but not Rust-only spellings like `inf`.
pub fn java_parse_double(s: &str) -> Option<f64> {
    let s = s.trim();
    let s = s.strip_suffix(['d', 'D', 'f', 'F']).unwrap_or(s);
    let unsigned = s.strip_prefix(['+', '-']).unwrap_or(s);
    if unsigned == "NaN" || unsigned == "Infinity" {
        return s.replace("Infinity", "inf").parse().ok();
    }
    if !unsigned.starts_with(|c: char| c.is_ascii_digit() || c == '.') {
        return None;
    }
    s.parse().ok()
}

/// Lenient number parsing equivalent to Java's `NumberFormat.getNumberInstance(Locale.ROOT).parse(str)`: parses the
/// longest numeric prefix (allowing `,` grouping separators and an `E` exponent) and ignores the rest.
fn lenient_parse_number(s: &str) -> Option<f64> {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut out = String::new();
    if bytes.first() == Some(&b'-') {
        out.push('-');
        i += 1;
    }
    let mut digits = 0;
    while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b',') {
        if bytes[i] != b',' {
            out.push(bytes[i] as char);
            digits += 1;
        }
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'.' {
        out.push('.');
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            out.push(bytes[i] as char);
            digits += 1;
            i += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    if i < bytes.len() && bytes[i] == b'E' {
        let mut j = i + 1;
        let mut exp = String::from("e");
        if j < bytes.len() && (bytes[j] == b'-' || bytes[j] == b'+') {
            exp.push(bytes[j] as char);
            j += 1;
        }
        let start = j;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            exp.push(bytes[j] as char);
            j += 1;
        }
        if j > start {
            out.push_str(&exp);
        }
    }
    out.parse().ok()
}

/// Java `Number.longValue()` from a double (saturating, NaN -> 0).
fn double_to_long(d: f64) -> i64 {
    d as i64
}

/// Returns `tag` as a string or `None` if null.
pub fn parse_string_or_null(tag: &Value) -> Option<String> {
    if tag.is_null() { None } else { Some(tag.to_string_lossy().into_owned()) }
}

/// Returns `tag` as a long or `None` if invalid.
pub fn parse_long_or_null(tag: &Value) -> Option<i64> {
    match tag {
        Value::Null => None,
        Value::Int(i) => Some(*i),
        Value::Float(f) => Some(double_to_long(*f as f64)),
        Value::Double(d) => Some(double_to_long(*d)),
        other => parse_long_or_null_str(&other.to_string_lossy()),
    }
}

/// Returns `tag` as a long or `None` if invalid.
pub fn parse_long_or_null_str(tag: &str) -> Option<i64> {
    tag.parse::<i64>().ok().or_else(|| lenient_parse_number(tag).map(double_to_long))
}

/// Returns `tag` as a long or 0 if invalid.
pub fn parse_long(tag: &Value) -> i64 {
    parse_long_or_null(tag).unwrap_or(0)
}

/// Returns `tag` as a long or 0 if invalid.
pub fn parse_long_str(tag: &str) -> i64 {
    parse_long_or_null_str(tag).unwrap_or(0)
}

/// Returns `tag` as an integer or `None` if invalid, ignoring any non-numeric suffix.
pub fn parse_int_substring(tag: &str) -> Option<i32> {
    INT_SUBSTRING_PATTERN.captures(tag).and_then(|c| c[1].parse().ok())
}

/// Returns `tag` as a number rounded to the nearest integer or `None` if invalid.
pub fn parse_round_int(tag: &Value) -> Option<i32> {
    if tag.is_null() {
        return None;
    }
    let s = tag.to_string_lossy();
    let caps = TO_ROUND_INT_SUBSTRING_PATTERN.captures(&s)?;
    let f: f32 = java_parse_double(&caps[1])? as f32;
    // Java Math.round(float) = floor(x + 0.5)
    Some((f + 0.5).floor() as i32)
}

/// Returns `tag` as an integer or `None` if invalid.
pub fn parse_int_or_null(tag: &Value) -> Option<i32> {
    match tag {
        Value::Null => None,
        Value::Int(i) => Some(*i as i32),
        Value::Float(f) => Some(*f as i32),
        Value::Double(d) => Some(*d as i32),
        other => parse_int_or_null_str(&other.to_string_lossy()),
    }
}

/// Returns `tag` as an integer or `None` if invalid.
pub fn parse_int_or_null_str(tag: &str) -> Option<i32> {
    tag.parse::<i32>().ok().or_else(|| lenient_parse_number(tag).map(|d| d as i32))
}

/// Returns `tag` parsed as a boolean.
pub fn bool(tag: &Value) -> bool {
    imposm3_parsers::bool(tag)
}

/// Returns `tag` parsed as an integer where 1 is true, 0 is false.
pub fn bool_int(tag: &Value) -> i32 {
    imposm3_parsers::bool_int(tag)
}

/// Returns 1 for forward one-way, -1 for reverse, 0 otherwise.
pub fn direction(input: &Value) -> i32 {
    imposm3_parsers::direction(input)
}

/// Returns a z-order for an OSM road based on the tags that are present. Bridges are above roads appear above tunnels
/// and major roads appear above minor.
pub fn wayzorder(tags: &Tags) -> i32 {
    imposm3_parsers::wayzorder(tags)
}

/// Returns `tag` as a double or `None` if invalid.
pub fn parse_double_or_null(tag: &Value) -> Option<f64> {
    match tag {
        Value::Null => None,
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f as f64),
        Value::Double(d) => Some(*d),
        other => parse_double_or_null_str(&other.to_string_lossy()),
    }
}

/// Returns `tag` as a double or `None` if invalid.
pub fn parse_double_or_null_str(tag: &str) -> Option<f64> {
    java_parse_double(tag).or_else(|| lenient_parse_number(tag))
}

/// Parses a value for the `-Xmx/-Xms` JVM option like "100" or "500k" or "15g" into bytes.
pub fn jvm_memory_string_to_bytes(value: &str) -> anyhow::Result<i64> {
    let err = || anyhow::anyhow!("Unable to parse size: {value}");
    let v = value.trim();
    let last = v.chars().last().ok_or_else(err)?;
    if last.is_ascii_digit() {
        return v.parse().map_err(|_| err());
    }
    let base: i64 = v[..v.len() - last.len_utf8()].parse().map_err(|_| err())?;
    Ok(match last.to_ascii_lowercase() {
        'k' => base * 1024,
        'm' => base * 1024 * 1024,
        'g' => base * 1024 * 1024 * 1024,
        _ => return Err(err()),
    })
}

/// Parses `tag` as a measure of distance with unit, converted to meters or `None` if invalid.
///
/// See [Map features/Units](https://wiki.openstreetmap.org/wiki/Map_features/Units) for the list of supported units.
pub fn meters(tag: &Value) -> Option<f64> {
    match tag {
        Value::Null => None,
        Value::Int(_) | Value::Float(_) | Value::Double(_) => tag.as_f64(),
        other => meters_str(&other.to_string_lossy()),
    }
}

/// Parses a string as a measure of distance with unit, converted to meters or `None` if invalid.
pub fn meters_str(s: &str) -> Option<f64> {
    let mut matches = DISTANCE.captures_iter(s);
    let caps = matches.next()?;
    let mut value = java_parse_double(&caps["value"])?;
    if caps.name("m").is_some() {
        // value *= 1
    } else if caps.name("km").is_some() {
        value *= 1000.0;
    } else if caps.name("mi").is_some() {
        value *= 1609.344;
    } else if caps.name("nmi").is_some() {
        value *= 1852.0;
    } else if caps.name("ft").is_some() {
        value *= 12.0 * 0.0254;
        // handle 15'3"
        if let Some(next) = matches.next()
            && next.name("in").is_some()
        {
            value += java_parse_double(&next["value"])? * 0.0254;
        }
    } else if caps.name("in").is_some() {
        value *= 0.0254;
    } else {
        return None;
    }
    Some(value)
}

/// Parses a value containing bandwidth, with units of kbps, mb/s, kib/s, etc.
///
/// Returned value is in units of bytes per second, or 0 if no limit specified.
pub fn bandwidth(tag: &Value) -> anyhow::Result<f64> {
    match tag {
        Value::Null => Ok(0.0),
        Value::Int(_) | Value::Float(_) | Value::Double(_) => Ok(tag.as_f64().unwrap_or(0.0)),
        other => bandwidth_str(&other.to_string_lossy()),
    }
}

/// Parses a string containing bandwidth, with units of kbps, mb/s, kib/s, etc.
///
/// Returned value is in units of bytes per second, or 0 if no limit specified (empty string).
pub fn bandwidth_str(s: &str) -> anyhow::Result<f64> {
    let err = || anyhow::anyhow!("Unable to parse bandwidth: {s}");
    let Some(caps) = NUMBER_WITH_UNIT.captures(s) else {
        return Ok(0.0);
    };
    let value = java_parse_double(&caps["value"]).ok_or_else(err)?;
    let unit: String = caps["unit"].to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
    let multiplier = match unit.as_str() {
        "b/s" | "" => 1.0,
        "kb/s" => 1_000.0,
        "mb/s" => 1_000_000.0,
        "gb/s" => 1_000_000_000.0,
        "bps" => 1.0 / 8.0,
        "kbps" => 1_000.0 / 8.0,
        "mbps" => 1_000_000.0 / 8.0,
        "gbps" => 1_000_000_000.0 / 8.0,
        "kib/s" => (1 << 10) as f64,
        "mib/s" => (1 << 20) as f64,
        "gib/s" => (1 << 30) as f64,
        _ => return Err(err()),
    };
    let result = value * multiplier;
    if result < 0.0 {
        return Err(err());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_boolean() {
        for (input, out, bool_int_out) in [
            ("0", false, 0),
            ("false", false, 0),
            ("no", false, 0),
            ("yes", true, 1),
            ("true", true, 1),
            ("ok", true, 1),
        ] {
            assert_eq!(out, bool(&Value::str(input)), "{input}");
            assert_eq!(bool_int_out, bool_int(&Value::str(input)), "{input}");
        }
    }

    #[test]
    fn test_long() {
        for (input, out, obj) in
            [("0", 0, Some(0)), ("false", 0, None), ("123", 123, Some(123)), ("123.123", 123, Some(123))]
        {
            assert_eq!(out, parse_long(&Value::str(input)), "{input}");
            assert_eq!(obj, parse_long_or_null(&Value::str(input)), "{input}");
        }
    }

    #[test]
    fn test_direction() {
        for (input, out) in [("1", 1), ("yes", 1), ("true", 1), ("-1", -1), ("2", 0), ("0", 0)] {
            assert_eq!(out, direction(&Value::str(input)), "{input}");
        }
    }

    #[test]
    fn test_length() {
        let cases: &[(&str, Option<f64>)] = &[
            ("1", Some(1.0)),
            ("100", Some(100.0)),
            ("-1.23 m", Some(-1.23)),
            ("100.2", Some(100.2)),
            ("1m", Some(1.0)),
            ("1meter", Some(1.0)),
            ("100 meters", Some(100.0)),
            ("1.5m", Some(1.5)),
            ("1km", Some(1000.0)),
            ("0.2km", Some(200.0)),
            ("0.2 km", Some(200.0)),
            ("1mi", Some(1609.344)),
            ("1 mi", Some(1609.344)),
            ("328'", Some(99.974)),
            ("328ft", Some(99.974)),
            ("328'11\"", Some(100.254)),
            ("328ft 11in", Some(100.254)),
            ("garbage", None),
            ("1nmi", Some(1852.0)),
            ("1.5 nmi", Some(2778.0)),
            ("1..5 nmi", None),
            ("36\"", Some(0.9144)),
            ("1'11\"", Some(0.584)),
            ("132.74'", Some(40.4592)),
            ("132'8.88\"", Some(40.4592)),
            ("1'11m", Some(0.305)),
            ("1.5 smoots", None),
        ];
        for (input, expected) in cases {
            let result = meters(&Value::str(*input));
            match expected {
                None => assert_eq!(None, result, "{input}"),
                Some(e) => {
                    let r = result.unwrap_or_else(|| panic!("{input} returned None"));
                    assert!((r - e).abs() < 1e-3, "{input}: {r} != {e}");
                }
            }
        }
    }

    #[test]
    fn test_int_substring() {
        let cases: &[(&str, Option<i32>)] = &[
            ("1", Some(1)),
            ("0", Some(0)),
            ("-1", Some(-1)),
            ("1.1", Some(1)),
            ("-1.1", Some(-1)),
            ("-1.23 m", Some(-1)),
            ("one meter", None),
        ];
        for (input, out) in cases {
            assert_eq!(*out, parse_int_substring(input), "{input}");
        }
    }

    #[test]
    fn test_parse_jvm_size() {
        for (input, out) in
            [("0", 0), ("1", 1), ("999999999999", 999999999999), ("2k", 2048), ("4M", 4194304), ("8G", 8589934592)]
        {
            assert_eq!(out, jvm_memory_string_to_bytes(input).unwrap(), "{input}");
        }
        for input in ["123p", "123gk", "garbage", ""] {
            assert!(jvm_memory_string_to_bytes(input).is_err(), "{input}");
        }
    }

    #[test]
    fn test_parse_bandwidth() {
        for (input, out) in [
            ("0", 0.0),
            ("1", 1.0),
            ("999999999999", 999999999999.0),
            ("2b/s", 2.0),
            ("2kib/s", 2048.0),
            ("4mib/s", 4194304.0),
            ("8GiB/s", 8589934592.0),
            ("2kb/s", 2000.0),
            ("4mb/s", 4000000.0),
            ("8Gb/s", 8000000000.0),
            ("2bps", 0.25),
            ("8bps", 1.0),
            ("16bps", 2.0),
            ("8kbps", 1000.0),
            ("8mbps", 1000000.0),
            ("8gbps", 1000000000.0),
            ("garbage", 0.0),
            ("", 0.0),
        ] {
            assert_eq!(out, bandwidth_str(input).unwrap(), "{input}");
        }
        assert!(bandwidth_str("1 smoot/s").is_err());
    }

    #[test]
    fn test_numbers() {
        assert_eq!(Some(1.5), parse_double_or_null(&Value::str("1.5")));
        assert_eq!(Some(1234.0), parse_double_or_null(&Value::str("1,234")));
        assert_eq!(None, parse_double_or_null(&Value::str("inf")));
        assert_eq!(Some(12), parse_int_or_null(&Value::str("12abc")));
        assert_eq!(Some(3), parse_int_or_null(&Value::Double(3.7)));
        assert_eq!(Some(4), parse_round_int(&Value::str("3.5")));
        assert_eq!(None, parse_round_int(&Value::Null));
    }
}

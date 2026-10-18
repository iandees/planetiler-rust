//! Port of `reader/WithTags.java`: an input element with a set of string key/object value pairs.
//!
//! Note: [`WithTags::get_tag`] is a direct map lookup that returns a reference. Java's `getTag` also falls back to
//! dotted-key struct access (`a.b`, `a[].b`) when the key is missing; that behavior is exposed through
//! [`WithTags::get_tag_nested`], which returns a [`Cow`] since nested values may need to be computed.
//! [`WithTags::has_tag`], [`WithTags::get_string`] and the other helpers use the dotted-key-aware lookup.

use std::borrow::Cow;

use crate::reader::structs::{Struct, StructKey};
use crate::value::{Tags, Value};

/// Something that has tags, equivalent to Java's `WithTags`.
pub trait WithTags {
    fn tags(&self) -> &Tags;

    /// Value of a tag, or `None` if missing (direct lookup, no dotted-key fallback).
    fn get_tag(&self, key: &str) -> Option<&Value> {
        self.tags().get(key)
    }

    /// Equivalent of Java's `getTag(key)`: value of a tag, or if missing and `key` contains a `.`, the nested value
    /// using [`Struct`] traversal (`a.b`, `a[].b`). Null values are treated as missing.
    fn get_tag_nested(&self, key: &str) -> Option<Cow<'_, Value>> {
        match self.tags().get(key) {
            Some(v) if !v.is_null() => Some(Cow::Borrowed(v)),
            _ if key.contains('.') => {
                let v = get_dotted(self, key).into_raw_value();
                if v.is_null() { None } else { Some(Cow::Owned(v)) }
            }
            _ => None,
        }
    }

    /// Value of a tag, or `default` if missing.
    fn get_tag_or<'a>(&'a self, key: &str, default: &'a Value) -> &'a Value {
        self.get_tag(key).unwrap_or(default)
    }

    /// True if the key is present, or the dotted path refers to a non-null nested value.
    fn has_tag(&self, key: &str) -> bool {
        self.tags().contains_key(key) || (key.contains('.') && !get_dotted(self, key).is_null())
    }

    /// True if the tag equals `value`, or if either is a list and they share an element.
    fn has_tag_value(&self, key: &str, value: &Value) -> bool {
        self.get_tag_nested(key).is_some_and(|actual| contains(&actual, value))
    }

    /// Shorthand for [`WithTags::has_tag_value`] with a string value.
    fn has_tag_str(&self, key: &str, value: &str) -> bool {
        match self.get_tag_nested(key).as_deref() {
            Some(Value::String(s)) => &**s == value,
            Some(other) => contains(other, &Value::str(value)),
            None => false,
        }
    }

    /// True if the tag matches any of `values`.
    fn has_tag_any(&self, key: &str, values: &[&str]) -> bool {
        match self.get_tag_nested(key).as_deref() {
            Some(Value::String(s)) => values.contains(&&**s),
            Some(other) => values.iter().any(|v| contains(other, &Value::str(v))),
            None => false,
        }
    }

    /// True if the tag matches any of `values`.
    fn has_tag_any_value(&self, key: &str, values: &[Value]) -> bool {
        self.get_tag_nested(key).is_some_and(|actual| values.iter().any(|v| contains(&actual, v)))
    }

    /// String form of a tag value (numbers are formatted like Java's `toString`).
    fn get_string(&self, key: &str) -> Option<String> {
        self.get_tag_nested(key).map(|v| v.to_string_lossy().into_owned())
    }

    fn get_string_or(&self, key: &str, default: &str) -> String {
        self.get_string(key).unwrap_or_else(|| default.to_string())
    }

    /// `false` if the tag's string value is missing, empty, "0", "false", or "no" and `true` otherwise.
    fn get_boolean(&self, key: &str) -> bool {
        parse_shim::bool(self.get_tag_nested(key).as_deref())
    }

    /// The value for `key` parsed as a long, or 0 if missing or invalid.
    fn get_long(&self, key: &str) -> i64 {
        parse_shim::parse_long(self.get_tag_nested(key).as_deref())
    }

    /// The value for `key` interpreted as a direction, where -1 is reverse, 1 is forward, and 0 is other.
    fn get_direction(&self, key: &str) -> i32 {
        parse_shim::direction(self.get_tag_nested(key).as_deref())
    }

    /// A z-order for an OSM road based on the tags that are present (bridges above roads above tunnels, major roads
    /// above minor).
    fn get_way_zorder(&self) -> i32 {
        parse_shim::wayzorder(self.tags())
    }

    /// A [`Struct`] wrapper for a field, which can be a primitive or nested list/map.
    fn get_struct(&self, key: &str) -> Struct {
        Struct::from(self.get_tag_nested(key).map(Cow::into_owned).unwrap_or_default())
    }

    /// Shortcut for calling [`Struct::get_key`] multiple times to get a deeply nested value.
    fn get_struct_path(&self, key: &str, others: &[StructKey<'_>]) -> Struct {
        self.get_struct(key).get_path(others)
    }

    /// Attempts to deserialize the tags on this feature into a typed struct using serde.
    fn as_type<T: serde::de::DeserializeOwned>(&self) -> Option<T>
    where
        Self: Sized,
    {
        crate::reader::json_conversion::convert_value(&Value::Map(std::sync::Arc::new(self.tags().clone())))
    }

    /// Serialize the tags to a JSON string.
    fn tags_as_json(&self) -> String {
        crate::reader::json_conversion::to_json(&Value::Map(std::sync::Arc::new(self.tags().clone()))).to_string()
    }
}

fn get_dotted<T: WithTags + ?Sized>(this: &T, key: &str) -> Struct {
    // split on the first "." or "[]."
    match key.split_once('.') {
        Some((first, rest)) => {
            let first = first.strip_suffix("[]").unwrap_or(first);
            this.get_struct(first).get(rest)
        }
        None => this.get_struct(key),
    }
}

fn contains(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::List(actual), Value::List(expected)) => expected.iter().any(|e| actual.contains(e)),
        (Value::List(actual), expected) => actual.contains(expected),
        (actual, Value::List(expected)) => expected.contains(actual),
        (actual, expected) => actual == expected,
    }
}

/// Wraps a [`Tags`] map to implement [`WithTags`] (Java's `WithTags.OfMap`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OfMap(pub Tags);

impl WithTags for OfMap {
    fn tags(&self) -> &Tags {
        &self.0
    }
}

impl WithTags for Tags {
    fn tags(&self) -> &Tags {
        self
    }
}

/// Minimal private copies of the `util::Parse`/`Imposm3Parsers` helpers needed by `WithTags`, `Struct` and
/// `expression::DataType`.
///
/// TODO(port): switch to `crate::util::parse` / `crate::util::imposm3_parsers` once they are ported.
pub(crate) mod parse_shim {
    use crate::value::{Tags, Value};

    /// Java `Imposm3Parsers.bool`: false for missing, "", "0", "false", "no".
    pub fn bool(tag: Option<&Value>) -> bool {
        match tag {
            None | Some(Value::Null) => false,
            Some(v) => bool_str(&v.to_string_lossy()),
        }
    }

    pub fn bool_str(s: &str) -> bool {
        !matches!(s, "" | "0" | "false" | "no")
    }

    /// Java `Imposm3Parsers.direction`.
    pub fn direction(tag: Option<&Value>) -> i32 {
        match tag {
            None | Some(Value::Null) => 0,
            Some(v) => match &*v.to_string_lossy() {
                "1" | "yes" | "true" => 1,
                "-1" => -1,
                _ => 0,
            },
        }
    }

    /// Java `Parse.parseLong`: 0 when missing or invalid.
    pub fn parse_long(tag: Option<&Value>) -> i64 {
        parse_long_or_null_value(tag).unwrap_or(0)
    }

    pub fn parse_long_or_null_value(tag: Option<&Value>) -> Option<i64> {
        match tag? {
            Value::Null => None,
            Value::Int(i) => Some(*i),
            Value::Float(f) => Some(*f as i64),
            Value::Double(d) => Some(*d as i64),
            other => parse_long_or_null(&other.to_string_lossy()),
        }
    }

    pub fn parse_int_or_null_value(tag: Option<&Value>) -> Option<i32> {
        match tag? {
            Value::Null => None,
            Value::Int(i) => Some(*i as i32),
            Value::Float(f) => Some(*f as i32),
            Value::Double(d) => Some(*d as i32),
            other => parse_int_or_null(&other.to_string_lossy()),
        }
    }

    pub fn parse_double_or_null_value(tag: Option<&Value>) -> Option<f64> {
        match tag? {
            Value::Null => None,
            v if v.is_number() => v.as_f64(),
            other => parse_double_or_null(&other.to_string_lossy()),
        }
    }

    pub fn parse_long_or_null(s: &str) -> Option<i64> {
        s.parse::<i64>().ok().or_else(|| number_prefix(s).map(|d| d as i64))
    }

    pub fn parse_int_or_null(s: &str) -> Option<i32> {
        s.parse::<i32>().ok().or_else(|| number_prefix(s).map(|d| d as i64 as i32))
    }

    pub fn parse_double_or_null(s: &str) -> Option<f64> {
        let t = s.trim();
        let t = t.strip_suffix(['d', 'D', 'f', 'F']).unwrap_or(t);
        match t {
            "NaN" => Some(f64::NAN),
            "Infinity" | "+Infinity" => Some(f64::INFINITY),
            "-Infinity" => Some(f64::NEG_INFINITY),
            _ if t.chars().any(|c| c.is_ascii_alphabetic() && c != 'e' && c != 'E') => number_prefix(s),
            _ => t.parse::<f64>().ok().or_else(|| number_prefix(s)),
        }
    }

    /// Approximates Java's `NumberFormat.getNumberInstance(Locale.ROOT).parse`: parses the longest numeric prefix
    /// (optional `-`, digits with `,` grouping, optional fraction and exponent).
    fn number_prefix(s: &str) -> Option<f64> {
        let bytes = s.as_bytes();
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        if bytes.first() == Some(&b'-') {
            out.push('-');
            i += 1;
        }
        let mut digits = 0;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || (bytes[i] == b',' && digits > 0)) {
            if bytes[i] != b',' {
                out.push(bytes[i] as char);
                digits += 1;
            }
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'.' {
            let mut frac = String::from(".");
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                frac.push(bytes[j] as char);
                j += 1;
            }
            if frac.len() > 1 || digits > 0 {
                digits += frac.len() - 1;
                out.push_str(&frac);
                i = j;
            }
        }
        if digits == 0 {
            return None;
        }
        if i < bytes.len() && bytes[i] == b'E' {
            let mut exp = String::from("e");
            let mut j = i + 1;
            if j < bytes.len() && bytes[j] == b'-' {
                exp.push('-');
                j += 1;
            }
            let start = exp.len();
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                exp.push(bytes[j] as char);
                j += 1;
            }
            if exp.len() > start {
                out.push_str(&exp);
            }
        }
        out.parse().ok()
    }

    fn default_rank(highway: &str) -> Option<i64> {
        Some(match highway {
            "minor" | "road" | "unclassified" | "residential" | "tertiary_link" | "secondary_link" | "primary_link"
            | "trunk_link" | "motorway_link" => 3,
            "tertiary" => 4,
            "secondary" => 5,
            "primary" => 6,
            "trunk" => 8,
            "motorway" => 9,
            _ => return None,
        })
    }

    /// Java `Imposm3Parsers.wayzorder`.
    pub fn wayzorder(tags: &Tags) -> i32 {
        let rank = tags
            .get("highway")
            .filter(|v| !v.is_null())
            .and_then(|v| default_rank(&v.to_string_lossy()))
            .unwrap_or(if tags.contains_key("railway") { 7 } else { 0 });
        let bool_int = |k: &str| if bool(tags.get(k)) { 1i64 } else { 0 };
        let z = parse_long(tags.get("layer")) * 10 + rank + bool_int("tunnel") * -10 + bool_int("bridge") * 10;
        if z.abs() < 10_000 { z as i32 } else { 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tags;

    #[test]
    fn has_tag() {
        let t = OfMap(tags! {"a" => "b", "n" => 1, "l" => vec!["x", "y"]});
        assert!(t.has_tag("a"));
        assert!(t.has_tag_str("a", "b"));
        assert!(!t.has_tag_str("a", "c"));
        assert!(t.has_tag_any("a", &["c", "b"]));
        assert!(t.has_tag_value("n", &Value::Int(1)));
        assert!(t.has_tag_str("l", "y"));
        assert!(t.has_tag_any("l", &["z", "y"]));
        assert_eq!(Some("1".to_string()), t.get_string("n"));
    }

    #[test]
    fn dotted() {
        let t = OfMap(tags! {"a" => Value::map(tags!{"b" => "c", "l" => vec![1, 2]}), "x.y" => "z"});
        assert!(t.has_tag("a.b"));
        assert!(!t.has_tag("a.c"));
        assert!(t.has_tag_str("a.b", "c"));
        assert_eq!(Some("z".to_string()), t.get_string("x.y"));
        assert_eq!(Some(Value::from(vec![1, 2])), t.get_tag_nested("a.l").map(|c| c.into_owned()));
        assert!(t.get_tag("a.b").is_none());
        let list = OfMap(tags! {"a" => Value::list([Value::map(tags!{"b" => 1}), Value::map(tags!{"b" => 2})])});
        assert_eq!(Some(Value::from(vec![1, 2])), list.get_tag_nested("a[].b").map(|c| c.into_owned()));
    }

    #[test]
    fn parse_helpers() {
        let t = OfMap(tags! {
          "t" => "yes", "f" => "no", "z" => "0", "n" => 12, "s" => "12", "d" => "1.5", "bad" => "abc",
          "o1" => "-1", "o2" => "true"
        });
        assert!(t.get_boolean("t"));
        assert!(!t.get_boolean("f"));
        assert!(!t.get_boolean("z"));
        assert!(!t.get_boolean("missing"));
        assert_eq!(12, t.get_long("n"));
        assert_eq!(12, t.get_long("s"));
        assert_eq!(1, t.get_long("d"));
        assert_eq!(0, t.get_long("bad"));
        assert_eq!(-1, t.get_direction("o1"));
        assert_eq!(1, t.get_direction("o2"));
        assert_eq!(0, t.get_direction("bad"));
        assert_eq!(Some(1000), parse_shim::parse_long_or_null("1,000"));
        assert_eq!(Some(1.5), parse_shim::parse_double_or_null("1.5"));
        assert_eq!(None, parse_shim::parse_double_or_null("abc"));
    }

    #[test]
    fn wayzorder() {
        assert_eq!(6, OfMap(tags! {"highway" => "primary"}).get_way_zorder());
        assert_eq!(16, OfMap(tags! {"highway" => "primary", "bridge" => "yes"}).get_way_zorder());
        assert_eq!(-4, OfMap(tags! {"highway" => "primary", "tunnel" => "1"}).get_way_zorder());
        assert_eq!(17, OfMap(tags! {"railway" => "rail", "layer" => "1"}).get_way_zorder());
        assert_eq!(0, OfMap(tags! {"layer" => "10000"}).get_way_zorder());
    }
}

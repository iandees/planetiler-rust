//! Port of `reader/Struct.java`: forgiving, typed access to nested values (primitives, lists and maps).
//!
//! The APIs are meant to be forgiving, so if you access field `a.b.c` but `a` is missing from the top-level struct it
//! will just return [`Struct::NULL`] instead of failing. Values are coerced to other types when possible:
//! - [`Struct::as_long`]/[`Struct::as_double`] on a string attempt to parse it as a number
//! - [`Struct::get`] on a string attempts to parse it as JSON and traverse nested data
//! - [`Struct::as_string`] on anything returns a string representation of it
//! - [`Struct::as_json`] returns a JSON representation of the underlying value
//!
//! In Rust a struct is a thin wrapper around a [`Value`] (cloning is cheap since nested values are reference counted).
//!
//! TODO(port): Java also supports `Instant`/`LocalDate`/`LocalTime`/`UUID` inputs; [`Value`] has no such variants.
//! TODO(port): Java caches the parsed JSON of string values; here it is re-parsed on each access.

use std::fmt;

use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde::de::DeserializeOwned;

use crate::reader::json_conversion;
use crate::util::{imposm3_parsers, parse};
use crate::value::{Value, java_double_to_string};

/// A key used to traverse a [`Struct`]: either a map key or a list index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StructKey<'a> {
    Key(&'a str),
    Index(i64),
}

impl<'a> From<&'a str> for StructKey<'a> {
    fn from(value: &'a str) -> Self {
        StructKey::Key(value)
    }
}

impl<'a> From<&'a String> for StructKey<'a> {
    fn from(value: &'a String) -> Self {
        StructKey::Key(value)
    }
}

impl From<i32> for StructKey<'_> {
    fn from(value: i32) -> Self {
        StructKey::Index(value as i64)
    }
}

impl From<i64> for StructKey<'_> {
    fn from(value: i64) -> Self {
        StructKey::Index(value)
    }
}

impl From<usize> for StructKey<'_> {
    fn from(value: usize) -> Self {
        StructKey::Index(value as i64)
    }
}

/// Wrapper for a value that could either be a primitive, list, or map of nested primitives.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Struct(Value);

impl From<Value> for Struct {
    fn from(value: Value) -> Self {
        Struct(value)
    }
}

impl From<&Value> for Struct {
    fn from(value: &Value) -> Self {
        Struct(value.clone())
    }
}

impl From<Option<&Value>> for Struct {
    fn from(value: Option<&Value>) -> Self {
        Struct(value.cloned().unwrap_or_default())
    }
}

impl Struct {
    /// A missing or empty value.
    pub const NULL: Struct = Struct(Value::Null);

    /// Returns a new struct that wraps a primitive value, or nested list or map of values.
    pub fn of(value: impl Into<Value>) -> Struct {
        Struct(value.into())
    }

    /// Returns the nested field of a map struct, elements of a list (flat-mapped), or [`Struct::NULL`] when called on a
    /// primitive value. Dotted keys like `a.b` and `a[].b` traverse nested structures. Strings are parsed as JSON.
    pub fn get(&self, key: &str) -> Struct {
        match &self.0 {
            Value::Map(map) => {
                if let Some(v) = map.get(key)
                    && !v.is_null()
                {
                    return Struct(v.clone());
                }
                if let Some((first, rest)) = key.split_once('.') {
                    return match first.strip_suffix("[]") {
                        Some(list_key) => self.get(list_key).flat_map(|child| child.get(rest)),
                        None => {
                            let nested = self.get(first);
                            if nested.is_null() { Struct::NULL } else { nested.get(rest) }
                        }
                    };
                }
                Struct::NULL
            }
            Value::List(_) => {
                let stripped = key.strip_prefix("[]").map(|k| k.strip_prefix('.').unwrap_or(k)).unwrap_or(key);
                self.flat_map(|v| v.get(stripped))
            }
            Value::String(s) => parse_json(s).get(key),
            _ => Struct::NULL,
        }
    }

    /// Returns the nth element of a list, or [`Struct::NULL`] when not a list or `index` is out of bounds.
    pub fn get_index(&self, index: i64) -> Struct {
        match &self.0 {
            Value::List(list) => {
                if index >= 0 && (index as usize) < list.len() {
                    Struct(list[index as usize].clone())
                } else {
                    Struct::NULL
                }
            }
            Value::String(s) => parse_json(s).get_index(index),
            _ => Struct::NULL,
        }
    }

    /// Same as [`Struct::get`] or [`Struct::get_index`] depending on the type of key.
    pub fn get_key(&self, key: StructKey<'_>) -> Struct {
        match key {
            StructKey::Key(k) => self.get(k),
            StructKey::Index(i) => self.get_index(i),
        }
    }

    /// Shortcut for calling [`Struct::get_key`] multiple times to query a value several layers deep.
    pub fn get_path(&self, path: &[StructKey<'_>]) -> Struct {
        let mut result = self.clone();
        for key in path {
            result = result.get_key(*key);
            if result.is_null() {
                return Struct::NULL;
            }
        }
        result
    }

    /// When this is a map, returns a map from key to (non-null) value struct, otherwise an empty map.
    pub fn as_map(&self) -> IndexMap<String, Struct> {
        match &self.0 {
            Value::Map(map) => {
                map.iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k.clone(), Struct(v.clone()))).collect()
            }
            Value::String(s) => parse_json(s).as_map(),
            _ => IndexMap::new(),
        }
    }

    /// Returns this struct, or `fallback` when [`Struct::NULL`].
    pub fn or_else(self, fallback: impl Into<Struct>) -> Struct {
        if self.is_null() { fallback.into() } else { self }
    }

    /// Returns the list of nested structs in a list, a list of this single element when this is a primitive, or an
    /// empty list when [`Struct::NULL`].
    pub fn as_list(&self) -> Vec<Struct> {
        match &self.0 {
            Value::Null => vec![],
            Value::List(list) => list.iter().map(|v| Struct(v.clone())).collect(),
            _ => vec![self.clone()],
        }
    }

    /// Integer value (truncating) for numeric values, or attempts to parse as a number when this is a string.
    pub fn as_int(&self) -> Option<i32> {
        match &self.0 {
            Value::Int(i) => Some(*i as i32),
            Value::Float(f) => Some(*f as i32),
            Value::Double(d) => Some(*d as i32),
            Value::String(s) => parse::parse_int_or_null_str(s),
            _ => None,
        }
    }

    /// Long value (truncating) for numeric values, or attempts to parse as a number when this is a string.
    pub fn as_long(&self) -> Option<i64> {
        match &self.0 {
            Value::Int(i) => Some(*i),
            Value::Float(f) => Some(*f as i64),
            Value::Double(d) => Some(*d as i64),
            Value::String(s) => parse::parse_long_or_null_str(s),
            _ => None,
        }
    }

    /// Double value for numeric values, or attempts to parse as a double when this is a string.
    pub fn as_double(&self) -> Option<f64> {
        match &self.0 {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f as f64),
            Value::Double(d) => Some(*d),
            Value::String(s) => parse::parse_double_or_null_str(s),
            _ => None,
        }
    }

    /// Boolean value of this element, or `true` for strings except "", "0", "false", and "no". Other types are `false`.
    pub fn as_boolean(&self) -> bool {
        match &self.0 {
            Value::Bool(b) => *b,
            Value::String(s) => imposm3_parsers::bool_str(s),
            _ => false,
        }
    }

    /// String representation of this value (use [`Struct::as_json`] for JSON), or `None` when null.
    pub fn as_string(&self) -> Option<String> {
        match &self.0 {
            Value::Null => None,
            Value::String(s) => Some(s.to_string()),
            Value::Map(_) => json_conversion::write_value_as_string(&self.0),
            Value::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
            Value::Float(f) => Some(java_float_to_string(*f)),
            other => Some(other.to_string()),
        }
    }

    /// Timestamp parsed from milliseconds since epoch, or a string with an ISO-8601 encoded time.
    pub fn as_timestamp(&self) -> Option<DateTime<Utc>> {
        match &self.0 {
            Value::Int(i) => DateTime::from_timestamp_millis(*i),
            Value::Float(_) | Value::Double(_) => {
                let d = self.as_double()?;
                let millis = d as i64;
                let base = DateTime::from_timestamp_millis(millis)?;
                let nanos = ((d - d.floor()) * 1_000_000.0) as i64;
                Some(base + chrono::Duration::nanoseconds(nanos))
            }
            Value::String(s) => DateTime::parse_from_rfc3339(s)
                .map(|d| d.with_timezone(&Utc))
                .ok()
                .or_else(|| self.as_long().and_then(DateTime::from_timestamp_millis)),
            _ => None,
        }
    }

    /// Byte array value, or bytes from a UTF8-encoded string.
    pub fn as_bytes(&self) -> Option<Vec<u8>> {
        match &self.0 {
            Value::Bytes(b) => Some(b.to_vec()),
            Value::String(s) => Some(s.as_bytes().to_vec()),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        self.0.is_null()
    }

    /// True if this is a map with nested key/value pairs, false for lists or primitives.
    pub fn is_struct(&self) -> bool {
        matches!(self.0, Value::Map(_))
    }

    /// The raw primitive, list or map value.
    pub fn raw_value(&self) -> &Value {
        &self.0
    }

    pub fn into_raw_value(self) -> Value {
        self.0
    }

    /// Attempts to deserialize this value into a typed struct using serde. JSON strings are parsed.
    pub fn as_type<T: DeserializeOwned>(&self) -> Option<T> {
        match &self.0 {
            Value::String(s) => json_conversion::read_value(s),
            other => json_conversion::convert_value(other),
        }
    }

    /// JSON string representation of the raw value wrapped by this struct.
    pub fn as_json(&self) -> String {
        json_conversion::write_value_as_string(&self.0).unwrap_or_else(|| "null".to_string())
    }

    /// Returns a new list where each element of this list has been expanded to the list of elements returned by
    /// `mapper`. Individual items are treated as a list containing just that item.
    pub fn flat_map(&self, mapper: impl Fn(&Struct) -> Struct) -> Struct {
        let mut result = Vec::new();
        for item in self.as_list() {
            match mapper(&item).0 {
                Value::Null => {}
                Value::List(list) => result.extend(list.iter().cloned()),
                other => result.push(other),
            }
        }
        if result.is_empty() { Struct::NULL } else { Struct(Value::list(result)) }
    }
}

impl fmt::Display for Struct {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.as_string() {
            Some(s) => f.write_str(&s),
            None => f.write_str("null"),
        }
    }
}

fn parse_json(s: &str) -> Struct {
    Struct(json_conversion::read_value_dynamic(s).unwrap_or_default())
}

/// Formats a float the way Java's `Float.toString` does for typical values.
pub(crate) fn java_float_to_string(f: f32) -> String {
    let abs = f.abs();
    if f.is_finite() && f != 0.0 && (1e-3..1e7).contains(&abs) {
        let s = format!("{f}");
        if s.contains('.') { s } else { format!("{s}.0") }
    } else {
        java_double_to_string(f as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::with_tags::{OfMap, WithTags};
    use crate::tags;

    fn map(t: crate::value::Tags) -> Value {
        Value::map(t)
    }

    #[test]
    fn test_get() {
        let s = OfMap(tags! {"a" => map(tags!{"b" => "c"})});
        let nested = s.get_struct("a").get("b");
        assert_eq!(Some("c".into()), nested.as_string());
        assert_eq!(&Value::str("c"), nested.raw_value());
        assert!(!nested.is_null());
        assert!(s.get_struct("a").get("b").get("c").get("d").is_null());
    }

    #[test]
    fn test_get_multilevel() {
        let s = OfMap(tags! {"a" => map(tags!{"b" => "c"})});
        let nested = s.get_struct_path("a", &["b".into()]);
        assert_eq!(Some("c".into()), nested.as_string());
        assert!(s.get_struct_path("a", &["b".into(), "c".into(), "d".into()]).is_null());
    }

    #[test]
    fn test_get_dotted_from_struct() {
        assert_eq!(Some("c".into()), Struct::of(map(tags! {"a" => map(tags!{"b" => "c"})})).get("a.b").as_string());
        assert_eq!(Some("c".into()), Struct::of(map(tags! {"a.b" => "c"})).get("a.b").as_string());
        assert_eq!(Some("d".into()), Struct::of(map(tags! {"a" => map(tags!{"b.c" => "d"})})).get("a.b.c").as_string());
        assert_eq!(None, Struct::of(map(tags! {"a" => map(tags!{"b.c" => "d"})})).get("a.b.e").as_string());
    }

    #[test]
    fn test_get_dotted_from_with_tags() {
        let s = OfMap(tags! {"a" => map(tags!{"b" => "c"})});
        assert_eq!(Some("c".into()), s.get_struct("a.b").as_string());
        assert_eq!(Some(Value::str("c")), s.get_tag_nested("a.b").map(|c| c.into_owned()));
        assert!(s.has_tag("a.b"));
    }

    #[test]
    fn test_list_query() {
        let s = Struct::of(map(tags! {"a" => Value::list([map(tags!{"b" => "c"}), map(tags!{"b" => "d"})])}));
        assert_eq!(Some("d".into()), s.get("a").flat_map(|e| e.get("b")).get_index(1).as_string());
        assert_eq!(Struct::of(vec!["c", "d"]), s.get("a[].b"));
    }

    #[test]
    fn test_list_query_on_list() {
        let s = Struct::of(Value::list([map(tags! {"b" => "c"}), map(tags! {"b" => "d"})]));
        assert_eq!(Some("d".into()), s.flat_map(|e| e.get("b")).get_index(1).as_string());
        assert_eq!(Struct::of(vec!["c", "d"]), s.get("[].b"));
        assert_eq!(Struct::of(vec!["c", "d"]), s.get("b"));
        assert_eq!(Struct::NULL, s.get("e"));
    }

    #[test]
    fn test_list_get() {
        let s = Struct::of(vec![1, 2, 3]);
        assert_eq!(Some(1), s.get_index(0).as_int());
        assert_eq!(Some(3), s.get_index(2).as_int());
        assert!(s.get_index(4).is_null());
        assert!(s.get_index(-1).is_null());
    }

    #[test]
    fn test_null_input() {
        let s = Struct::of(Value::Null);
        assert!(s.raw_value().is_null());
        assert!(s.is_null());
        assert!(s.get_index(0).is_null());
        assert!(s.get("nested").is_null());
        assert!(s.get_path(&["nested".into(), "level2".into()]).is_null());
        assert!(s.as_map().is_empty());
        assert!(s.as_list().is_empty());
        assert_eq!("null", s.to_string());
        assert_eq!("null", s.as_json());
        #[derive(serde::Deserialize)]
        struct Type {}
        assert!(s.as_type::<Type>().is_none());
        assert_eq!(&Value::Int(1), s.or_else(Struct::of(1)).raw_value());
    }

    fn assert_not_list_or_map(s: &Struct) {
        assert!(s.get_index(0).is_null());
        assert!(s.get("nested").is_null());
        assert!(s.get_path(&["nested".into(), "level2".into()]).is_null());
        assert!(s.as_map().is_empty());
        assert_eq!(vec![s.clone()], s.as_list());
    }

    #[test]
    fn test_boolean_input() {
        let s = Struct::of(true);
        assert_eq!(&Value::Bool(true), s.raw_value());
        assert!(s.as_boolean());
        assert!(!s.is_null());
        assert_not_list_or_map(&s);
        assert_eq!("true", s.to_string());
        assert_eq!("true", s.as_json());
        assert_eq!(&Value::Bool(true), s.or_else(Struct::of(1)).raw_value());
    }

    #[test]
    fn test_int_input() {
        let s = Struct::of(1);
        assert_eq!(&Value::Int(1), s.raw_value());
        assert_eq!(Some(1), s.as_int());
        assert_eq!(Some(1), s.as_long());
        assert_eq!(Some(1.0), s.as_double());
        assert_not_list_or_map(&s);
        assert_eq!(Some(1), s.as_list()[0].as_int());
        assert_eq!("1", s.to_string());
        assert_eq!("1", s.as_json());
        assert_eq!(DateTime::from_timestamp_millis(1), s.as_timestamp());
        assert_eq!(&Value::Int(1), s.or_else(Struct::of(2)).raw_value());
    }

    #[test]
    fn test_float_input() {
        let s = Struct::of(1.3f32);
        assert_eq!(&Value::Float(1.3), s.raw_value());
        assert_eq!(Some(1), s.as_int());
        assert_eq!(Some(1), s.as_long());
        assert!((s.as_double().unwrap() - 1.3).abs() < 1e-2);
        assert_not_list_or_map(&s);
        assert_eq!("1.3", s.to_string());
        assert_eq!("1.3", s.as_json());
    }

    #[test]
    fn test_double_input() {
        let s = Struct::of(1.3f64);
        assert_eq!(&Value::Double(1.3), s.raw_value());
        assert_eq!(Some(1), s.as_int());
        assert_eq!(Some(1), s.as_long());
        assert_eq!(Some(1.3), s.as_double());
        assert_not_list_or_map(&s);
        assert_eq!("1.3", s.to_string());
        assert_eq!("1.3", s.as_json());
    }

    #[test]
    fn test_numbers_convert_to_timestamps() {
        let expected = DateTime::from_timestamp(1, 500_000).unwrap();
        assert_eq!(Some(expected), Struct::of(1000.5).as_timestamp());
        assert_eq!(DateTime::from_timestamp_millis(1500), Struct::of(1500).as_timestamp());
    }

    #[test]
    fn test_string_input() {
        let s = Struct::of("abc");
        assert_not_list_or_map(&s);
        assert_eq!(Some("abc".into()), s.as_string());
        assert_eq!("\"abc\"", s.as_json());
        assert_eq!(None, s.as_int());
        assert_eq!(None, s.as_long());
        assert_eq!(None, s.as_double());
        assert!(s.as_boolean());
        assert_eq!(Some(b"abc".to_vec()), s.as_bytes());
    }

    #[test]
    fn test_string_to_number() {
        let s = Struct::of("1.5");
        assert_eq!(Some(1), s.as_int());
        assert_eq!(Some(1), s.as_long());
        assert_eq!(Some(1.5), s.as_double());
        assert!(s.as_boolean());
        assert_eq!("\"1.5\"", s.as_json());
    }

    #[test]
    fn test_string_to_boolean() {
        assert!(!Struct::of("false").as_boolean());
        assert!(Struct::of("true").as_boolean());
        assert!(Struct::of("yes").as_boolean());
        assert!(!Struct::of("0").as_boolean());
        assert!(Struct::of("1").as_boolean());
        assert!(!Struct::of("no").as_boolean());
    }

    #[test]
    fn test_string_to_instant() {
        let t = DateTime::from_timestamp(100, 0).unwrap();
        assert_eq!(Some(t), Struct::of("1970-01-01T00:01:40Z").as_timestamp());
        assert_eq!(Some(t), Struct::of("100000").as_timestamp());
    }

    #[test]
    fn test_json_string_to_struct() {
        #[derive(serde::Deserialize, PartialEq, Debug)]
        struct Inner {
            b: i32,
        }
        #[derive(serde::Deserialize, PartialEq, Debug)]
        struct Outer {
            a: Vec<Inner>,
        }
        let s = Struct::of("{\"a\":[{\"b\":1}]}\n");
        assert_eq!(Some(1), s.get_path(&["a".into(), 0.into(), "b".into()]).as_int());
        assert_eq!(Some(Outer { a: vec![Inner { b: 1 }] }), s.as_type::<Outer>());
    }

    #[test]
    fn test_json_list_to_struct() {
        let s = Struct::of("[1,2,3]\n");
        assert_eq!(Some(1), s.get_index(0).as_int());
        assert_eq!(Some(2), s.get_index(1).as_int());
    }

    #[test]
    fn test_binary_input() {
        let s = Struct::of(Value::Bytes(vec![1u8, 2].into()));
        assert_eq!(Some(vec![1, 2]), s.as_bytes());
    }

    #[test]
    fn test_as_mapper() {
        #[derive(serde::Deserialize, PartialEq, Debug)]
        struct Inner {
            b: String,
        }
        #[derive(serde::Deserialize, PartialEq, Debug)]
        struct Outer {
            a: Inner,
        }
        let s = OfMap(tags! {"a" => map(tags!{"b" => "c"})});
        assert_eq!(Some(Outer { a: Inner { b: "c".into() } }), s.as_type::<Outer>());
        assert_eq!(Some(Inner { b: "c".into() }), s.get_struct("a").as_type::<Inner>());
    }

    #[test]
    fn test_as_json() {
        let s = OfMap(tags! {"a" => map(tags!{"b" => "c"})});
        assert_eq!(r#"{"a":{"b":"c"}}"#, s.tags_as_json());
        assert_eq!(r#"{"b":"c"}"#, s.get_struct("a").as_json());
        assert_eq!(r#""c""#, s.get_struct("a").get("b").as_json());
    }

    #[test]
    fn test_raw_value_list() {
        assert_eq!(&Value::from(vec![1, 2, 3]), Struct::of(vec![1, 2, 3]).raw_value());
    }

    #[test]
    fn test_raw_value_list_flatmap() {
        let s = Struct::of(Value::list([Value::Int(1), Value::from(vec![3])]));
        assert_eq!(&Value::from(vec![1, 3]), s.flat_map(|d| d.clone()).raw_value());
        assert_eq!(&Value::from(vec![3]), s.flat_map(|d| d.get_index(0)).raw_value());
    }
}

//! Port of `reader/WithTags.java`: an input element with a set of string key/object value pairs.
//!
//! TODO(port): dotted-key struct access (`getTag("a.b")`), `getStruct`, and the `Parse`-based helpers
//! (`getBoolean`, `getLong`, `getDirection`, `getWayZorder`).

use crate::value::{Tags, Value};

/// Something that has tags, equivalent to Java's `WithTags`.
pub trait WithTags {
    fn tags(&self) -> &Tags;

    /// Value of a tag, or `None` if missing.
    fn get_tag(&self, key: &str) -> Option<&Value> {
        self.tags().get(key)
    }

    /// Value of a tag, or `default` if missing.
    fn get_tag_or<'a>(&'a self, key: &str, default: &'a Value) -> &'a Value {
        self.get_tag(key).unwrap_or(default)
    }

    fn has_tag(&self, key: &str) -> bool {
        self.tags().contains_key(key)
    }

    /// True if the tag equals `value`, or if either is a list and they share an element.
    fn has_tag_value(&self, key: &str, value: &Value) -> bool {
        self.get_tag(key).is_some_and(|actual| contains(actual, value))
    }

    /// Shorthand for [`WithTags::has_tag_value`] with a string value.
    fn has_tag_str(&self, key: &str, value: &str) -> bool {
        match self.get_tag(key) {
            Some(Value::String(s)) => &**s == value,
            Some(other) => contains(other, &Value::str(value)),
            None => false,
        }
    }

    /// True if the tag matches any of `values`.
    fn has_tag_any(&self, key: &str, values: &[&str]) -> bool {
        values.iter().any(|v| self.has_tag_str(key, v))
    }

    /// String form of a tag value (numbers are formatted like Java's `toString`).
    fn get_string(&self, key: &str) -> Option<String> {
        self.get_tag(key).map(|v| v.to_string_lossy().into_owned())
    }

    fn get_string_or(&self, key: &str, default: &str) -> String {
        self.get_string(key).unwrap_or_else(|| default.to_string())
    }

    /// Serialize the tags to a JSON string.
    fn tags_as_json(&self) -> String {
        Value::Map(std::sync::Arc::new(self.tags().clone())).to_json().to_string()
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
        assert_eq!(Some("1".to_string()), t.get_string("n"));
    }
}

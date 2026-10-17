//! Dynamically-typed attribute values.
//!
//! Java Planetiler stores tags and vector tile attributes as `Map<String, Object>`. In Rust we use [`Value`] for the
//! values and [`Tags`] (an insertion-ordered map) for the maps.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use indexmap::IndexMap;

/// Insertion-ordered map of attribute key to value, equivalent to Java's `Map<String, Object>`.
pub type Tags = IndexMap<String, Value>;

/// A dynamically-typed attribute value, equivalent to the `Object` values used for tags in Java Planetiler.
///
/// Integer types (`Integer`/`Long` in Java) are collapsed into [`Value::Int`], but `Float` and `Double` remain distinct
/// because they are encoded differently in vector tiles.
#[derive(Clone, Debug, Default)]
pub enum Value {
    #[default]
    Null,
    Bool(bool),
    Int(i64),
    Float(f32),
    Double(f64),
    String(Arc<str>),
    List(Arc<Vec<Value>>),
    Map(Arc<Tags>),
    Bytes(Arc<[u8]>),
}

impl Value {
    pub fn str(s: impl AsRef<str>) -> Value {
        Value::String(Arc::from(s.as_ref()))
    }

    pub fn list(items: impl IntoIterator<Item = Value>) -> Value {
        Value::List(Arc::new(items.into_iter().collect()))
    }

    pub fn map(items: Tags) -> Value {
        Value::Map(Arc::new(items))
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Returns the integer value, without parsing strings or truncating floats.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// Returns any numeric value as a double, without parsing strings.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f as f64),
            Value::Double(d) => Some(*d),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&Tags> {
        match self {
            Value::Map(m) => Some(m),
            _ => None,
        }
    }

    pub fn is_number(&self) -> bool {
        matches!(self, Value::Int(_) | Value::Float(_) | Value::Double(_))
    }

    /// String form of this value, matching Java's `Object.toString()` for the common types (`null` for [`Value::Null`]).
    pub fn to_string_lossy(&self) -> Cow<'_, str> {
        match self {
            Value::String(s) => Cow::Borrowed(s),
            other => Cow::Owned(other.to_string()),
        }
    }

    /// Convert to a [`serde_json::Value`].
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::Value as J;
        match self {
            Value::Null => J::Null,
            Value::Bool(b) => J::Bool(*b),
            Value::Int(i) => J::from(*i),
            Value::Float(f) => serde_json::Number::from_f64(*f as f64).map_or(J::Null, J::Number),
            Value::Double(d) => serde_json::Number::from_f64(*d).map_or(J::Null, J::Number),
            Value::String(s) => J::String(s.to_string()),
            Value::List(l) => J::Array(l.iter().map(Value::to_json).collect()),
            Value::Map(m) => J::Object(m.iter().map(|(k, v)| (k.clone(), v.to_json())).collect()),
            Value::Bytes(b) => J::Array(b.iter().map(|b| J::from(*b)).collect()),
        }
    }

    /// Convert from a [`serde_json::Value`].
    pub fn from_json(json: &serde_json::Value) -> Value {
        use serde_json::Value as J;
        match json {
            J::Null => Value::Null,
            J::Bool(b) => Value::Bool(*b),
            J::Number(n) => n.as_i64().map(Value::Int).unwrap_or_else(|| Value::Double(n.as_f64().unwrap_or(f64::NAN))),
            J::String(s) => Value::str(s),
            J::Array(a) => Value::list(a.iter().map(Value::from_json)),
            J::Object(o) => Value::map(o.iter().map(|(k, v)| (k.clone(), Value::from_json(v))).collect()),
        }
    }
}

/// Formats a double the way Java's `Double.toString` does for typical values (`1.0`, `1.5`, `1.0E10`).
pub fn java_double_to_string(d: f64) -> String {
    if d.is_nan() {
        "NaN".into()
    } else if d.is_infinite() {
        if d > 0.0 { "Infinity".into() } else { "-Infinity".into() }
    } else if d == 0.0 {
        if d.is_sign_negative() { "-0.0".into() } else { "0.0".into() }
    } else {
        let abs = d.abs();
        if (1e-3..1e7).contains(&abs) {
            let s = format!("{d}");
            if s.contains('.') { s } else { format!("{s}.0") }
        } else {
            let s = format!("{d:E}");
            // rust: 1E10 / 1.5E-5 -> java: 1.0E10 / 1.5E-5
            match s.split_once('E') {
                Some((mantissa, exp)) if !mantissa.contains('.') => format!("{mantissa}.0E{exp}"),
                _ => s,
            }
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(v) => f.write_str(&java_double_to_string(*v as f64)),
            Value::Double(v) => f.write_str(&java_double_to_string(*v)),
            Value::String(s) => f.write_str(s),
            Value::List(l) => {
                f.write_str("[")?;
                for (i, v) in l.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{v}")?;
                }
                f.write_str("]")
            }
            Value::Map(m) => {
                f.write_str("{")?;
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{k}={v}")?;
                }
                f.write_str("}")
            }
            Value::Bytes(b) => write!(f, "{b:?}"),
        }
    }
}

impl PartialEq for Value {
    /// Java `equals` semantics: values of different types are never equal (except all integer widths), doubles compare
    /// by bits so that `Value` can be used as a hash map key.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a.to_bits() == b.to_bits(),
            (Value::Double(a), Value::Double(b)) => a.to_bits() == b.to_bits(),
            (Value::String(a), Value::String(b)) => a == b,
            (Value::List(a), Value::List(b)) => a == b,
            (Value::Map(a), Value::Map(b)) => a == b,
            (Value::Bytes(a), Value::Bytes(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for Value {}

impl Hash for Value {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Value::Null => {}
            Value::Bool(b) => b.hash(state),
            Value::Int(i) => i.hash(state),
            Value::Float(v) => v.to_bits().hash(state),
            Value::Double(v) => v.to_bits().hash(state),
            Value::String(s) => s.hash(state),
            Value::List(l) => l.hash(state),
            Value::Map(m) => {
                for (k, v) in m.iter() {
                    k.hash(state);
                    v.hash(state);
                }
            }
            Value::Bytes(b) => b.hash(state),
        }
    }
}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        match (self, other) {
            (Value::String(a), Value::String(b)) => a.partial_cmp(b),
            (Value::Bool(a), Value::Bool(b)) => a.partial_cmp(b),
            (Value::Int(a), Value::Int(b)) => a.partial_cmp(b),
            (a, b) if a.is_number() && b.is_number() => a.as_f64()?.partial_cmp(&b.as_f64()?),
            (Value::Null, Value::Null) => Some(Ordering::Equal),
            _ => None,
        }
    }
}

macro_rules! from_impl {
    ($t:ty, $variant:ident, $conv:expr) => {
        impl From<$t> for Value {
            fn from(v: $t) -> Self {
                Value::$variant($conv(v))
            }
        }
    };
}

from_impl!(bool, Bool, |v| v);
from_impl!(i8, Int, |v: i8| v as i64);
from_impl!(i16, Int, |v: i16| v as i64);
from_impl!(i32, Int, |v: i32| v as i64);
from_impl!(i64, Int, |v| v);
from_impl!(u8, Int, |v: u8| v as i64);
from_impl!(u16, Int, |v: u16| v as i64);
from_impl!(u32, Int, |v: u32| v as i64);
from_impl!(usize, Int, |v: usize| v as i64);
from_impl!(f32, Float, |v| v);
from_impl!(f64, Double, |v| v);
from_impl!(&str, String, Arc::from);
from_impl!(String, String, Arc::from);
from_impl!(&String, String, |v: &String| Arc::from(v.as_str()));
from_impl!(Arc<str>, String, |v| v);
from_impl!(Tags, Map, Arc::new);

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        v.map_or(Value::Null, Into::into)
    }
}

impl<T: Into<Value>> From<Vec<T>> for Value {
    fn from(v: Vec<T>) -> Self {
        Value::list(v.into_iter().map(Into::into))
    }
}

/// Build a [`Tags`] map: `tags!{"highway" => "primary", "lanes" => 2}`.
#[macro_export]
macro_rules! tags {
  () => { $crate::value::Tags::new() };
  ($($k:expr => $v:expr),+ $(,)?) => {{
    let mut m = $crate::value::Tags::new();
    $(m.insert(::std::string::String::from($k), $crate::value::Value::from($v));)+
    m
  }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_matches_java() {
        assert_eq!("1.0", Value::Double(1.0).to_string());
        assert_eq!("1.5", Value::Double(1.5).to_string());
        assert_eq!("1.0E10", Value::Double(1e10).to_string());
        assert_eq!("1.5E-5", Value::Double(1.5e-5).to_string());
        assert_eq!("12", Value::Int(12).to_string());
        assert_eq!("null", Value::Null.to_string());
        assert_eq!("[1, a]", Value::list([Value::Int(1), Value::str("a")]).to_string());
    }

    #[test]
    fn tags_macro() {
        let t = tags! {"a" => 1, "b" => "c"};
        assert_eq!(Some(&Value::Int(1)), t.get("a"));
        assert_eq!(Some("c"), t.get("b").and_then(Value::as_str));
    }

    #[test]
    fn equality() {
        assert_eq!(Value::str("a"), Value::from("a"));
        assert_ne!(Value::Int(1), Value::Double(1.0));
    }
}

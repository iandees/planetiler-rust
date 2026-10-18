//! Port of `reader/JsonConversion.java`: utilities for converting between JSON strings and [`Value`]s / typed structs.
//!
//! Java uses Jackson; here we use `serde_json`, taking care to format floats the way Jackson does (`1.3f` -> `1.3`).

use serde::de::DeserializeOwned;

use crate::value::Value;

/// Converts a [`Value`] to a [`serde_json::Value`], formatting `f32` values using their shortest representation
/// (like Jackson does) instead of widening them to `f64`.
pub fn to_json(value: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match value {
        Value::Float(f) => {
            let widened: f64 = f.to_string().parse().unwrap_or(*f as f64);
            serde_json::Number::from_f64(widened).map_or(J::Null, J::Number)
        }
        Value::List(l) => J::Array(l.iter().map(to_json).collect()),
        Value::Map(m) => J::Object(m.iter().map(|(k, v)| (k.clone(), to_json(v))).collect()),
        other => other.to_json(),
    }
}

/// Serializes `value` to a JSON string, or `None` for [`Value::Null`] (Java returns `null`).
pub fn write_value_as_string(value: &Value) -> Option<String> {
    if value.is_null() { None } else { Some(to_json(value).to_string()) }
}

/// Converts `value` into a typed struct using serde (Java's `ObjectMapper.convertValue`), or `None` if `value` is
/// null or can't be converted.
pub fn convert_value<T: DeserializeOwned>(value: &Value) -> Option<T> {
    if value.is_null() {
        return None;
    }
    serde_json::from_value(to_json(value)).ok()
}

/// Parses a JSON string into a typed struct, or `None` if it is invalid.
pub fn read_value<T: DeserializeOwned>(string: &str) -> Option<T> {
    serde_json::from_str(string).ok()
}

/// Parses a JSON string into a [`Value`], or `None` if it is invalid.
pub fn read_value_dynamic(string: &str) -> Option<Value> {
    serde_json::from_str::<serde_json::Value>(string).ok().map(|j| Value::from_json(&j))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tags;

    #[test]
    fn floats_like_jackson() {
        assert_eq!(Some("1.3".to_string()), write_value_as_string(&Value::Float(1.3)));
        assert_eq!(Some("1.3".to_string()), write_value_as_string(&Value::Double(1.3)));
        assert_eq!(None, write_value_as_string(&Value::Null));
    }

    #[test]
    fn round_trip() {
        let v = Value::map(tags! {"a" => vec![1, 2], "b" => "c"});
        let s = write_value_as_string(&v).unwrap();
        assert_eq!(r#"{"a":[1,2],"b":"c"}"#, s);
        assert_eq!(Some(v), read_value_dynamic(&s));
        assert_eq!(None, read_value_dynamic("{bad"));
    }

    #[test]
    fn typed() {
        #[derive(serde::Deserialize, Debug, PartialEq)]
        struct Inner {
            b: String,
        }
        let v = Value::map(tags! {"b" => "c"});
        assert_eq!(Some(Inner { b: "c".into() }), convert_value(&v));
        assert_eq!(Some(Inner { b: "d".into() }), read_value(r#"{"b":"d"}"#));
    }
}

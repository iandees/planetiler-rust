//! Port of `expression/DataType.java`: destination data types for an attribute that link the type to functions that
//! can parse the value from an input object.

use crate::reader::with_tags::{WithTags, parse_shim};
use crate::value::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DataType {
    GetString,
    GetBoolean,
    GetDirection,
    GetLong,
    GetInt,
    GetDouble,
    GetTag,
}

impl DataType {
    pub const ALL: [DataType; 7] = [
        DataType::GetString,
        DataType::GetBoolean,
        DataType::GetDirection,
        DataType::GetLong,
        DataType::GetInt,
        DataType::GetDouble,
        DataType::GetTag,
    ];

    pub fn id(&self) -> &'static str {
        match self {
            DataType::GetString => "string",
            DataType::GetBoolean => "boolean",
            DataType::GetDirection => "direction",
            DataType::GetLong => "long",
            DataType::GetInt => "integer",
            DataType::GetDouble => "double",
            DataType::GetTag => "get",
        }
    }

    /// Returns the data type associated with `value`, or [`DataType::GetTag`] as a fallback.
    ///
    /// Java distinguishes `Integer` (GET_INT) and `Long` (GET_LONG); [`Value::Int`] maps to [`DataType::GetLong`].
    pub fn type_of(value: &Value) -> DataType {
        match value {
            Value::String(_) => DataType::GetString,
            Value::Int(_) => DataType::GetLong,
            Value::Double(_) => DataType::GetDouble,
            Value::Bool(_) => DataType::GetBoolean,
            _ => DataType::GetTag,
        }
    }

    /// Returns the data type associated with `id`, or [`DataType::GetTag`] as a fallback.
    pub fn from_id(id: &str) -> DataType {
        Self::ALL.into_iter().find(|d| d.id() == id).unwrap_or(DataType::GetTag)
    }

    /// Extracts the value of `key` from `input` as this type ([`Value::Null`] when missing/invalid).
    pub fn apply(&self, input: &dyn WithTags, key: &str) -> Value {
        match self {
            DataType::GetString => input.get_string(key).map_or(Value::Null, Value::from),
            DataType::GetBoolean => Value::Bool(input.get_boolean(key)),
            DataType::GetDirection => Value::Int(input.get_direction(key) as i64),
            DataType::GetLong => Value::Int(input.get_long(key)),
            DataType::GetInt | DataType::GetDouble => self.convert_from(input.get_tag_nested(key).as_deref()),
            DataType::GetTag => input.get_tag_nested(key).map(|c| c.into_owned()).unwrap_or_default(),
        }
    }

    /// Parses `value` into this type ([`Value::Null`] when missing/invalid).
    pub fn convert_from(&self, value: Option<&Value>) -> Value {
        let value = value.filter(|v| !v.is_null());
        match self {
            DataType::GetString => value.map_or(Value::Null, |v| Value::from(v.to_string_lossy().as_ref())),
            DataType::GetBoolean => Value::Bool(parse_shim::bool(value)),
            DataType::GetDirection => Value::Int(parse_shim::direction(value) as i64),
            DataType::GetLong => parse_shim::parse_long_or_null_value(value).map_or(Value::Null, Value::Int),
            DataType::GetInt => parse_shim::parse_int_or_null_value(value).map_or(Value::Null, Value::from),
            DataType::GetDouble => parse_shim::parse_double_or_null_value(value).map_or(Value::Null, Value::Double),
            DataType::GetTag => value.cloned().unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::with_tags::OfMap;
    use crate::tags;

    #[test]
    fn ids() {
        for d in DataType::ALL {
            assert_eq!(d, DataType::from_id(d.id()));
        }
        assert_eq!(DataType::GetTag, DataType::from_id("unknown"));
    }

    #[test]
    fn apply() {
        let t = OfMap(tags! {"a" => "1", "b" => "yes", "c" => "1.5"});
        assert_eq!(Value::str("1"), DataType::GetString.apply(&t, "a"));
        assert_eq!(Value::Null, DataType::GetString.apply(&t, "missing"));
        assert_eq!(Value::Bool(true), DataType::GetBoolean.apply(&t, "b"));
        assert_eq!(Value::Int(1), DataType::GetDirection.apply(&t, "b"));
        assert_eq!(Value::Int(1), DataType::GetLong.apply(&t, "a"));
        assert_eq!(Value::Int(0), DataType::GetLong.apply(&t, "missing"));
        assert_eq!(Value::Int(1), DataType::GetInt.apply(&t, "c"));
        assert_eq!(Value::Double(1.5), DataType::GetDouble.apply(&t, "c"));
        assert_eq!(Value::Null, DataType::GetDouble.apply(&t, "b"));
        assert_eq!(Value::str("1.5"), DataType::GetTag.apply(&t, "c"));
        assert_eq!(DataType::GetString, DataType::type_of(&Value::str("a")));
    }
}

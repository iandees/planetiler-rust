//! Port of `TypeConversion.java`: convert between types in a forgiving way (parse strings to get a number, call
//! `toString` to get a string, etc.).
//!
//! Java passes the desired output `Class`; here the output type is an [`OutType`]. Java's `Integer` and `Long` both
//! map to [`Value::Int`] (`Integer` truncates to 32 bits like `Number.intValue`).

use planetiler_core::util::parse;
use planetiler_core::value::{Value, java_double_to_string};

/// The output type of an expression (Java's `Class<O>` type parameter).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum OutType {
    /// Any value (`Object.class`), returned as-is.
    #[default]
    Object,
    Integer,
    Long,
    Double,
    String,
    Boolean,
}

impl OutType {
    pub fn name(&self) -> &'static str {
        match self {
            OutType::Object => "Object",
            OutType::Integer => "Integer",
            OutType::Long => "Long",
            OutType::Double => "Double",
            OutType::String => "String",
            OutType::Boolean => "Boolean",
        }
    }
}

/// Error thrown when there is no conversion between two types (Java `IllegalArgumentException`).
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[error("No conversion from {from} to {to}")]
pub struct ConversionError {
    pub from: &'static str,
    pub to: &'static str,
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "Boolean",
        Value::Int(_) => "Long",
        Value::Float(_) => "Float",
        Value::Double(_) => "Double",
        Value::String(_) => "String",
        Value::List(_) => "List",
        Value::Map(_) => "Map",
        Value::Bytes(_) => "byte[]",
    }
}

/// Java `Float.toString` for typical values.
fn float_to_string(f: f32) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    let s = format!("{f}");
    if s.contains('.') || s.contains('e') { s } else { format!("{s}.0") }
}

/// `TypeConversion.doubleToString`: whole numbers are printed without a decimal point.
pub fn double_to_string(d: f64) -> String {
    if d % 1.0 == 0.0 { format!("{}", d as i64) } else { java_double_to_string(d) }
}

/// Attempts to coerce `value` to `out` using the first registered conversion that applies.
///
/// Returns an error if there is no available conversion (i.e. a boolean to a number).
pub fn convert(value: Value, out: OutType) -> Result<Value, ConversionError> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    let err = |v: &Value| ConversionError { from: type_name(v), to: out.name() };
    Ok(match out {
        OutType::Object => value,
        OutType::Integer => match &value {
            Value::Int(i) => Value::Int(*i as i32 as i64),
            Value::Float(f) => Value::Int(*f as i32 as i64),
            Value::Double(d) => Value::Int(*d as i32 as i64),
            Value::String(s) => parse::parse_int_or_null_str(s).map_or(Value::Null, |i| Value::Int(i as i64)),
            other => return Err(err(other)),
        },
        OutType::Long => match &value {
            Value::Int(_) => value,
            Value::Float(f) => Value::Int(*f as i64),
            Value::Double(d) => Value::Int(*d as i64),
            Value::String(s) => parse::parse_long_or_null_str(s).map_or(Value::Null, Value::Int),
            other => return Err(err(other)),
        },
        OutType::Double => match &value {
            Value::Double(_) => value,
            Value::Int(i) => Value::Double(*i as f64),
            Value::Float(f) => Value::Double(*f as f64),
            Value::String(s) => parse::parse_double_or_null_str(s).map_or(Value::Null, Value::Double),
            other => return Err(err(other)),
        },
        OutType::Boolean => match &value {
            Value::Bool(_) => value,
            Value::Int(i) => Value::Bool(*i != 0),
            Value::Float(f) => Value::Bool((*f as f64).abs() > 2.0 * f64::from_bits(1)),
            Value::Double(d) => Value::Bool(d.abs() > 2.0 * f64::from_bits(1)),
            Value::String(s) => Value::Bool(parse::bool(&Value::str(s.to_lowercase()))),
            other => Value::Bool(parse::bool(other)),
        },
        OutType::String => match &value {
            Value::String(_) => value,
            Value::Double(d) => Value::str(double_to_string(*d)),
            Value::Float(f) => Value::str(float_to_string(*f)),
            other => Value::str(other.to_string()),
        },
    })
}

/// Same as [`convert`] but returns [`Value::Null`] when there is no conversion.
pub fn convert_or_null(value: Value, out: OutType) -> Value {
    convert(value, out).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(out: Value, clazz: OutType, inputs: Vec<Value>) {
        for input in inputs {
            assert_eq!(out, convert(input.clone(), clazz).unwrap(), "{input:?} -> {clazz:?}");
        }
    }

    #[test]
    fn test_conversion() {
        use Value as V;
        case(V::Int(1), OutType::Integer, vec![V::str("1"), V::Int(1), V::Double(1.1)]);
        case(V::Int(1), OutType::Long, vec![V::str("1"), V::Int(1), V::Double(1.1)]);
        case(
            V::Double(1.0),
            OutType::Double,
            vec![V::str("1"), V::str("1.0"), V::str("1e0"), V::Int(1), V::Double(1.0)],
        );
        case(V::Double(1.1), OutType::Double, vec![V::str("1.1"), V::Double(1.1)]);
        case(V::str("1"), OutType::String, vec![V::str("1"), V::Int(1), V::Double(1.0)]);
        case(V::str("1.1"), OutType::String, vec![V::str("1.1"), V::Double(1.1), V::Float(1.1)]);
        case(V::str("1000"), OutType::String, vec![V::Int(1000), V::Double(1000.0)]);
        case(V::str("NaN"), OutType::String, vec![V::Double(f64::NAN)]);
        case(V::Bool(true), OutType::Boolean, vec![V::Int(1), V::Double(1.0), V::str("true"), V::str("TRUE")]);
        case(
            V::Bool(false),
            OutType::Boolean,
            vec![V::Int(0), V::Double(0.0), V::str("false"), V::str("FALSE"), V::str("no")],
        );
        case(V::str("true"), OutType::String, vec![V::Bool(true)]);
        case(V::str("false"), OutType::String, vec![V::Bool(false)]);
        case(V::Null, OutType::String, vec![V::Null]);
        case(V::Null, OutType::Integer, vec![V::Null]);
    }

    #[test]
    fn no_conversion() {
        assert!(convert(Value::Bool(true), OutType::Integer).is_err());
        assert_eq!(Value::Null, convert(Value::str("garbage"), OutType::Integer).unwrap());
    }
}

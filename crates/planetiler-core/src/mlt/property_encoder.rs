//! Port of `org.maplibre.mlt.converter.encodings.PropertyEncoder` (plus the trivial `FloatEncoder` /
//! `DoubleEncoder`).

use std::borrow::Cow;

use super::Feature;
use super::converter::ColumnMapping;
use super::encoding::{
    LogicalLevelTechnique, PhysicalLevelTechnique, PhysicalStreamType, StreamMetadata, encode_boolean_rle,
    encode_boolean_stream, put_varint,
};
use super::integer_encoder::{encode_int_stream, encode_long_stream};
use super::metadata::{Column, ColumnType, ScalarType};
use super::string_encoder;
use crate::value::Value;

/// `PropertyEncoder.encodePropertyColumns`.
pub fn encode_property_columns(
    out: &mut Vec<u8>,
    property_columns: &[&Column],
    features: &[&Feature],
    fast_pfor: bool,
    use_fsst: bool,
    coerce_property_values: bool,
    column_mappings: &[ColumnMapping],
) -> anyhow::Result<()> {
    let mut i = 0;
    for column in property_columns {
        match &column.column_type {
            ColumnType::Scalar(_) => {
                // Indicate a missing property column in the tile with a zero for the number of streams
                if column.has_stream_count() && features.iter().all(|f| !f.properties.contains_key(&column.name)) {
                    out.push(0);
                    continue;
                }
                encode_scalar_property_column(
                    out,
                    column,
                    false,
                    features,
                    fast_pfor,
                    use_fsst,
                    coerce_property_values,
                )?;
            }
            ColumnType::Struct(children) => {
                let mapping = column_mappings
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("Missing column mapping nested property column {}", column.name))?;
                i += 1;
                encode_struct_property_column(out, features, use_fsst, &column.name, children, mapping, fast_pfor)?;
            }
            other => anyhow::bail!("The specified data type for the field is currently not supported: {other:?}"),
        }
    }
    Ok(())
}

fn encode_struct_property_column(
    out: &mut Vec<u8>,
    features: &[&Feature],
    use_fsst: bool,
    root_name: &str,
    children: &[Column],
    mapping: &ColumnMapping,
    fast_pfor: bool,
) -> anyhow::Result<()> {
    if !mapping.use_shared_dictionary_encoding {
        anyhow::bail!("Only shared dictionary encoding is currently supported for nested property columns");
    }
    let mut shared_dictionary: Vec<Vec<Option<&str>>> = Vec::with_capacity(children.len());
    for child in children {
        if child.scalar_type() != Some(ScalarType::String) {
            anyhow::bail!("Only fields of type String are currently supported as nested property columns");
        }
        let property_name = format!("{root_name}{}", child.name);
        shared_dictionary.push(
            features
                .iter()
                .map(|f| match f.properties.get(&property_name) {
                    Some(Value::String(s)) => Some(&**s),
                    Some(_) | None => None,
                })
                .collect(),
        );
    }
    if shared_dictionary.iter().all(Vec::is_empty) {
        // Set number of streams to zero if no property values are present in this tile
        out.push(0);
        return Ok(());
    }
    let (num_streams, encoded) = string_encoder::encode_shared_dictionary(&shared_dictionary, fast_pfor, use_fsst);
    put_varint(out, num_streams);
    out.extend_from_slice(&encoded);
    Ok(())
}

fn bool_value(v: Option<&Value>) -> Option<bool> {
    match v {
        Some(Value::Bool(b)) => Some(*b),
        _ => None,
    }
}

fn int_value(v: Option<&Value>) -> Option<i32> {
    match v {
        Some(Value::Int(i)) => i32::try_from(*i).ok(),
        _ => None,
    }
}

fn long_value(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Int(i)) => Some(*i),
        _ => None,
    }
}

fn float_value(v: Option<&Value>) -> Option<f32> {
    match v {
        Some(Value::Float(f)) => Some(*f),
        Some(Value::Double(d)) => Some(*d as f32),
        _ => None,
    }
}

fn double_value(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Double(d)) => Some(*d),
        Some(Value::Float(f)) => Some(*f as f64),
        _ => None,
    }
}

/// Java `Object.toString()` of a property value (used when coercing a mixed-type column to strings).
pub fn java_to_string(value: &Value) -> Cow<'_, str> {
    match value {
        Value::String(s) => Cow::Borrowed(s),
        Value::Float(f) => Cow::Owned(java_float_to_string(*f)),
        other => Cow::Owned(other.to_string()),
    }
}

/// Java `Float.toString`.
pub fn java_float_to_string(f: f32) -> String {
    if f.is_nan() {
        "NaN".into()
    } else if f.is_infinite() {
        if f > 0.0 { "Infinity".into() } else { "-Infinity".into() }
    } else if f == 0.0 {
        if f.is_sign_negative() { "-0.0".into() } else { "0.0".into() }
    } else {
        let abs = f.abs();
        if (1e-3..1e7).contains(&abs) {
            let s = format!("{f}");
            if s.contains('.') { s } else { format!("{s}.0") }
        } else {
            let s = format!("{f:E}");
            match s.split_once('E') {
                Some((mantissa, exp)) if !mantissa.contains('.') => format!("{mantissa}.0E{exp}"),
                _ => s,
            }
        }
    }
}

fn string_value<'a>(v: Option<&'a Value>, coerce: bool) -> Option<Cow<'a, str>> {
    match v {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(Cow::Borrowed(s)),
        Some(other) if coerce => Some(java_to_string(other)),
        Some(_) => None,
    }
}

/// `PropertyEncoder.encodeScalarPropertyColumn` (the public overload, also used for the ID column).
pub fn encode_scalar_property_column(
    out: &mut Vec<u8>,
    column: &Column,
    is_id: bool,
    features: &[&Feature],
    fast_pfor: bool,
    use_fsst: bool,
    coerce_property_values: bool,
) -> anyhow::Result<()> {
    let scalar_type = column.scalar_type().ok_or_else(|| anyhow::anyhow!("scalarType must not be null: {column:?}"))?;
    let name = column.name.as_str();
    match scalar_type {
        ScalarType::Boolean => encode_boolean_column(out, features, column),
        ScalarType::Int32 | ScalarType::Uint32 => {
            let signed = scalar_type == ScalarType::Int32;
            let mut values = Vec::with_capacity(features.len());
            let mut present = Vec::with_capacity(features.len());
            for f in features {
                // Force ID values to integer for this column.
                let v = if is_id { f.id.map(|id| id as i32) } else { int_value(f.properties.get(name)) };
                present.push(v.is_some());
                if let Some(v) = v {
                    values.push(v);
                }
            }
            if column.nullable {
                encode_boolean_stream(out, &present, PhysicalStreamType::Present);
            }
            encode_int_stream(out, &values, fast_pfor, signed, PhysicalStreamType::Data, None);
        }
        ScalarType::Int64 | ScalarType::Uint64 => {
            let signed = scalar_type == ScalarType::Int64;
            let mut values = Vec::with_capacity(features.len());
            let mut present = Vec::with_capacity(features.len());
            for f in features {
                let v = if is_id { f.id } else { long_value(f.properties.get(name)) };
                present.push(v.is_some());
                if let Some(v) = v {
                    values.push(v);
                }
            }
            if column.nullable {
                encode_boolean_stream(out, &present, PhysicalStreamType::Present);
            }
            encode_long_stream(out, &values, signed, PhysicalStreamType::Data, None);
        }
        ScalarType::Float => {
            let mut bytes = Vec::with_capacity(features.len() * 4);
            let mut present = Vec::with_capacity(features.len());
            let mut count = 0;
            for f in features {
                let v = float_value(f.properties.get(name));
                present.push(v.is_some());
                if let Some(v) = v {
                    bytes.extend_from_slice(&v.to_le_bytes());
                    count += 1;
                }
            }
            if column.nullable {
                encode_boolean_stream(out, &present, PhysicalStreamType::Present);
            }
            write_raw_data_stream(out, count, &bytes);
        }
        ScalarType::Double => {
            let mut bytes = Vec::with_capacity(features.len() * 8);
            let mut present = Vec::with_capacity(features.len());
            let mut count = 0;
            for f in features {
                let v = double_value(f.properties.get(name));
                present.push(v.is_some());
                if let Some(v) = v {
                    bytes.extend_from_slice(&v.to_le_bytes());
                    count += 1;
                }
            }
            if column.nullable {
                encode_boolean_stream(out, &present, PhysicalStreamType::Present);
            }
            write_raw_data_stream(out, count, &bytes);
        }
        ScalarType::String => {
            let raw: Vec<Option<Cow<'_, str>>> =
                features.iter().map(|f| string_value(f.properties.get(name), coerce_property_values)).collect();
            let values: Vec<&str> = raw.iter().filter_map(|v| v.as_deref()).collect();
            let (num_streams, encoded) = string_encoder::encode(&values, fast_pfor, use_fsst);
            let mut present_stream = Vec::new();
            if column.nullable {
                let present: Vec<bool> = raw.iter().map(Option::is_some).collect();
                encode_boolean_stream(&mut present_stream, &present, PhysicalStreamType::Present);
            }
            // Plus 1 for present stream
            put_varint(out, num_streams + u32::from(!present_stream.is_empty()));
            out.extend_from_slice(&present_stream);
            out.extend_from_slice(&encoded);
        }
        other => anyhow::bail!("The specified scalar data type is currently not supported: {other:?}"),
    }
    Ok(())
}

fn write_raw_data_stream(out: &mut Vec<u8>, count: usize, bytes: &[u8]) {
    StreamMetadata::new(
        PhysicalStreamType::Data,
        None,
        LogicalLevelTechnique::None,
        LogicalLevelTechnique::None,
        PhysicalLevelTechnique::None,
        count as u32,
        bytes.len() as u32,
    )
    .encode_into(out);
    out.extend_from_slice(bytes);
}

fn encode_boolean_column(out: &mut Vec<u8>, features: &[&Feature], column: &Column) {
    let mut data = Vec::with_capacity(features.len());
    let mut present = Vec::with_capacity(features.len());
    for f in features {
        let v = bool_value(f.properties.get(&column.name));
        present.push(v.is_some());
        if let Some(v) = v {
            data.push(v);
        }
    }
    let encoded_data = encode_boolean_rle(&data);
    if column.nullable {
        let encoded_present = encode_boolean_rle(&present);
        StreamMetadata::new(
            PhysicalStreamType::Present,
            None,
            LogicalLevelTechnique::Rle,
            LogicalLevelTechnique::None,
            PhysicalLevelTechnique::None,
            present.len() as u32,
            encoded_present.len() as u32,
        )
        .encode_into(out);
        out.extend_from_slice(&encoded_present);
    }
    StreamMetadata::new(
        PhysicalStreamType::Data,
        None,
        LogicalLevelTechnique::Rle,
        LogicalLevelTechnique::None,
        PhysicalLevelTechnique::None,
        data.len() as u32,
        encoded_data.len() as u32,
    )
    .encode_into(out);
    out.extend_from_slice(&encoded_data);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_to_string_matches_java() {
        assert_eq!("1.5", java_float_to_string(1.5));
        assert_eq!("1.1", java_float_to_string(1.1));
        assert_eq!("1.0E7", java_float_to_string(1e7));
        assert_eq!("1.0E-4", java_float_to_string(1e-4));
        assert_eq!("100.0", java_float_to_string(100.0));
        assert_eq!("-0.0", java_float_to_string(-0.0));
    }
}

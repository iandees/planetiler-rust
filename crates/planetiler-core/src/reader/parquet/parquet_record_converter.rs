//! Port of `reader/parquet/ParquetRecordConverter.java`: converts parquet records to tag maps, mapping all structs to
//! maps and handling [list and map nested types](https://github.com/apache/parquet-format/blob/master/LogicalTypes.md#nested-types)
//! into [`Value::List`] and [`Value::Map`].
//!
//! Java implements parquet-mr's record materializer API directly. Here the parquet crate decodes row groups into arrow
//! record batches (which already applies the parquet nested-type compatibility rules), and this converts each row of a
//! batch into [`Tags`]:
//! - null top-level columns and null struct fields are omitted (like Java, which only adds values that are present)
//! - null list elements are kept as [`Value::Null`]
//! - map entries with a null key or value are dropped; map keys are converted to strings
//! - legacy 2-level lists whose repeated group is named `array` or `list` and contains a single `element` /
//!   `array_element` field are unwrapped to a list of the element values
//! - primitives are converted by [`primitive_value`]

use anyhow::{Result, bail};
use arrow::array::{Array, ArrayRef, AsArray, MapArray, StructArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::basic::{LogicalType, Type as PhysicalType};
use parquet::schema::types::SchemaDescriptor;

use crate::reader::parquet::geo_arrow::{self, GeoArrowEncoding};
use crate::reader::parquet::geo_parquet_metadata::GeoParquetMetadata;
use crate::reader::parquet::parquet_primitive_converter::{PrimitiveHint, primitive_value};
use crate::value::{Tags, Value};

/// True if a list item field is a legacy repeated group wrapper that should be unwrapped.
fn unwrapped_list_item(item: &Field) -> Option<usize> {
    let name = item.name().to_ascii_lowercase();
    if name != "array" && name != "list" {
        return None;
    }
    match item.data_type() {
        DataType::Struct(fields) if fields.len() == 1 => {
            let child = fields[0].name().to_ascii_lowercase();
            (child == "element" || child == "array_element").then_some(0)
        }
        _ => None,
    }
}

fn list_items(values: &ArrayRef, item: &Field, range: std::ops::Range<usize>) -> Value {
    let unwrap = unwrapped_list_item(item);
    let mut result = Vec::with_capacity(range.len());
    for i in range {
        let value = match unwrap {
            Some(idx) => {
                let s = values.as_struct();
                if s.is_null(i) { None } else { array_value(s.column(idx), i) }
            }
            None => array_value(values, i),
        };
        result.push(value.unwrap_or(Value::Null));
    }
    Value::list(result)
}

fn struct_value(array: &StructArray, row: usize) -> Value {
    let mut map = Tags::with_capacity(array.num_columns());
    for (field, column) in array.fields().iter().zip(array.columns()) {
        if let Some(v) = array_value(column, row) {
            map.insert(field.name().clone(), v);
        }
    }
    Value::map(map)
}

fn map_value(array: &MapArray, row: usize) -> Value {
    let offsets = array.value_offsets();
    let (start, end) = (offsets[row] as usize, offsets[row + 1] as usize);
    let keys = array.keys();
    let values = array.values();
    let mut map = Tags::with_capacity(end - start);
    for i in start..end {
        if let (Some(k), Some(v)) = (array_value(keys, i), array_value(values, i))
            && !v.is_null()
        {
            map.insert(k.to_string_lossy().into_owned(), v);
        }
    }
    Value::map(map)
}

/// Converts the value at `row` of `array`, or `None` if it is null.
pub fn array_value(array: &ArrayRef, row: usize) -> Option<Value> {
    array_value_with_hint(array, row, PrimitiveHint::None)
}

fn array_value_with_hint(array: &ArrayRef, row: usize, hint: PrimitiveHint) -> Option<Value> {
    if array.is_null(row) {
        return None;
    }
    Some(match array.data_type() {
        DataType::List(item) => {
            let list = array.as_list::<i32>();
            let offsets = list.value_offsets();
            list_items(list.values(), item, offsets[row] as usize..offsets[row + 1] as usize)
        }
        DataType::LargeList(item) => {
            let list = array.as_list::<i64>();
            let offsets = list.value_offsets();
            list_items(list.values(), item, offsets[row] as usize..offsets[row + 1] as usize)
        }
        DataType::FixedSizeList(item, size) => {
            let list = array.as_fixed_size_list();
            let start = row * *size as usize;
            list_items(list.values(), item, start..start + *size as usize)
        }
        DataType::Struct(_) => struct_value(array.as_struct(), row),
        DataType::Map(_, _) => map_value(array.as_map(), row),
        DataType::Dictionary(_, _) => {
            let dict = array.as_any_dictionary();
            let key = dict.normalized_keys()[row];
            return array_value_with_hint(dict.values(), key, hint);
        }
        _ => primitive_value(array.as_ref(), row, hint)?,
    })
}

/// Converts rows of record batches with a given schema into tags.
#[derive(Clone, Debug)]
pub struct ParquetRecordConverter {
    hints: Vec<PrimitiveHint>,
}

impl ParquetRecordConverter {
    /// Creates a converter for batches with `arrow_schema` read from a parquet file with `parquet_schema`, validating
    /// that the primary geometry column has the type its geoparquet encoding requires.
    pub fn new(
        arrow_schema: &Schema,
        parquet_schema: &SchemaDescriptor,
        geoparquet: Option<&GeoParquetMetadata>,
    ) -> Result<ParquetRecordConverter> {
        let root = parquet_schema.root_schema();
        let hints = arrow_schema
            .fields()
            .iter()
            .map(|field| match root.get_fields().iter().find(|f| f.name() == field.name()) {
                Some(t) if t.is_primitive() => {
                    let info = t.get_basic_info();
                    if matches!(info.logical_type_ref(), Some(LogicalType::Uuid)) {
                        PrimitiveHint::Uuid
                    } else if t.get_physical_type() == PhysicalType::INT96 {
                        PrimitiveHint::Int96
                    } else {
                        PrimitiveHint::None
                    }
                }
                _ => PrimitiveHint::None,
            })
            .collect();
        if let Some(geoparquet) = geoparquet {
            validate_geometry_column(arrow_schema, parquet_schema, geoparquet)?;
        }
        Ok(ParquetRecordConverter { hints })
    }

    /// Converts `row` of `batch` to tags, skipping null columns.
    pub fn convert_row(&self, batch: &RecordBatch, row: usize) -> Tags {
        self.convert_row_skipping(batch, row, None)
    }

    /// Converts `row` of `batch` to tags, skipping null columns and column `skip`.
    pub fn convert_row_skipping(&self, batch: &RecordBatch, row: usize, skip: Option<usize>) -> Tags {
        let schema = batch.schema_ref();
        let mut tags = Tags::with_capacity(batch.num_columns());
        for (i, column) in batch.columns().iter().enumerate() {
            if Some(i) == skip {
                continue;
            }
            let hint = self.hints.get(i).copied().unwrap_or_default();
            if let Some(value) = array_value_with_hint(column, row, hint) {
                tags.insert(schema.field(i).name().clone(), value);
            }
        }
        tags
    }
}

fn validate_geometry_column(
    arrow_schema: &Schema,
    parquet_schema: &SchemaDescriptor,
    geoparquet: &GeoParquetMetadata,
) -> Result<()> {
    let primary = geoparquet.primary_column_metadata()?;
    let column = &geoparquet.primary_column;
    let encoding = primary.encoding.as_str();
    let Ok(field) = arrow_schema.field_with_name(column) else {
        bail!("geometry column {column} not found");
    };
    let parquet_field = parquet_schema.root_schema().get_fields().iter().find(|f| f.name() == column);
    let require = |condition: bool, message: String| if condition { Ok(()) } else { bail!(message) };
    match encoding {
        "WKT" => require(
            matches!(field.data_type(), DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View),
            format!("String type required for wkt-encoded geometry column {column} got: {}", field.data_type()),
        ),
        "WKB" => require(
            matches!(field.data_type(), DataType::Binary | DataType::LargeBinary | DataType::BinaryView)
                && parquet_field.is_none_or(|f| f.get_basic_info().logical_type_ref().is_none()),
            format!("Binary type required for wkb-encoded geometry column {column} got: {}", field.data_type()),
        ),
        other => match GeoArrowEncoding::parse(other) {
            Some(e) => require(
                geo_arrow::is_valid_type(e, field.data_type()),
                format!(
                    "{} type required for geoarrow {other} column {column} got: {}",
                    e.pretty_type(),
                    field.data_type()
                ),
            ),
            None => bail!("Unexpected geoparquet geometry encoding: {other}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow::array::*;
    use arrow::buffer::OffsetBuffer;
    use arrow::datatypes::{Fields, TimeUnit};

    use super::*;
    use crate::tags;

    fn convert(field: Field, array: ArrayRef) -> Option<Value> {
        let schema = Arc::new(Schema::new(vec![field]));
        let batch = RecordBatch::try_new(schema, vec![array]).unwrap();
        let converter = ParquetRecordConverter { hints: vec![PrimitiveHint::None] };
        converter.convert_row(&batch, 0).swap_remove("value")
    }

    fn primitive(array: ArrayRef) -> Option<Value> {
        convert(Field::new("value", array.data_type().clone(), true), array)
    }

    #[test]
    fn primitives() {
        assert_eq!(Some(Value::Int(1)), primitive(Arc::new(Int32Array::from(vec![1]))));
        assert_eq!(Some(Value::Int(256)), primitive(Arc::new(Int16Array::from(vec![256]))));
        assert_eq!(Some(Value::Int(i64::MAX)), primitive(Arc::new(Int64Array::from(vec![i64::MAX]))));
        assert_eq!(Some(Value::Int(i64::MAX)), primitive(Arc::new(UInt64Array::from(vec![i64::MAX as u64]))));
        assert_eq!(Some(Value::Bool(true)), primitive(Arc::new(BooleanArray::from(vec![true]))));
        assert_eq!(Some(Value::Double(1.0)), primitive(Arc::new(Float32Array::from(vec![1.0f32]))));
        assert_eq!(Some(Value::Double(1.5)), primitive(Arc::new(Float64Array::from(vec![1.5]))));
        assert_eq!(Some(Value::str("abc")), primitive(Arc::new(StringArray::from(vec!["abc"]))));
        assert_eq!(
            Some(Value::Bytes(vec![1u8, 2].into())),
            primitive(Arc::new(BinaryArray::from(vec![&[1u8, 2][..]])))
        );
        assert_eq!(Some(Value::str("1970-01-03")), primitive(Arc::new(Date32Array::from(vec![2]))));
        assert_eq!(None, primitive(Arc::new(Int32Array::from(vec![None]))));
    }

    #[test]
    fn decimals() {
        for (scale, precision, value, expected) in [(0, 1, 10, 10.0), (1, 9, 10, 1.0), (2, 9, 10, 0.1)] {
            let array = Decimal128Array::from(vec![value as i128]).with_precision_and_scale(precision, scale).unwrap();
            assert_eq!(Some(Value::Double(expected)), primitive(Arc::new(array)));
        }
    }

    #[test]
    fn time_and_timestamp() {
        assert_eq!(Some(Value::str("00:00:00.000001")), primitive(Arc::new(Time64MicrosecondArray::from(vec![1]))));
        assert_eq!(Some(Value::str("00:00:01")), primitive(Arc::new(Time32MillisecondArray::from(vec![1000]))));
        for (unit_value, expected) in [
            (Arc::new(TimestampMillisecondArray::from(vec![1])) as ArrayRef, "1970-01-01T00:00:00.001Z"),
            (Arc::new(TimestampMicrosecondArray::from(vec![1000])) as ArrayRef, "1970-01-01T00:00:00.001Z"),
            (Arc::new(TimestampNanosecondArray::from(vec![1_000_000])) as ArrayRef, "1970-01-01T00:00:00.001Z"),
            (
                Arc::new(TimestampMicrosecondArray::from(vec![1]).with_timezone("UTC")) as ArrayRef,
                "1970-01-01T00:00:00Z",
            ),
        ] {
            assert_eq!(Some(Value::str(expected)), primitive(unit_value));
        }
        let _ = TimeUnit::Millisecond;
    }

    #[test]
    fn lists_structs_and_maps() {
        let list = ListArray::from_iter_primitive::<arrow::datatypes::Int32Type, _, _>(vec![Some(vec![
            Some(1),
            None,
            Some(3),
        ])]);
        assert_eq!(Some(Value::list([Value::Int(1), Value::Null, Value::Int(3)])), primitive(Arc::new(list)));

        let fields = Fields::from(vec![Field::new("i", DataType::Int32, true), Field::new("j", DataType::Utf8, true)]);
        let s = StructArray::new(
            fields,
            vec![Arc::new(Int32Array::from(vec![42])), Arc::new(StringArray::from(vec![None::<&str>]))],
            None,
        );
        assert_eq!(Some(Value::map(tags! {"i" => 42})), primitive(Arc::new(s)));

        let mut builder = MapBuilder::new(None, Int32Builder::new(), StringBuilder::new());
        builder.keys().append_value(1);
        builder.values().append_value("one");
        builder.keys().append_value(2);
        builder.values().append_null();
        builder.append(true).unwrap();
        assert_eq!(Some(Value::map(tags! {"1" => "one"})), primitive(Arc::new(builder.finish())));
    }

    #[test]
    fn legacy_array_list() {
        let element = Field::new("element", DataType::Int32, true);
        let item = Arc::new(Field::new("array", DataType::Struct(Fields::from(vec![element.clone()])), false));
        let structs =
            StructArray::new(Fields::from(vec![element]), vec![Arc::new(Int32Array::from(vec![1, 2, 3]))], None);
        let list = ListArray::new(item, OffsetBuffer::new(vec![0, 3].into()), Arc::new(structs), None);
        assert_eq!(Some(Value::list([Value::Int(1), Value::Int(2), Value::Int(3)])), primitive(Arc::new(list)));
    }
}

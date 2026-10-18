//! Port of `reader/parquet/ParquetPrimitiveConverter.java`: converts typed primitive values from parquet records
//! (decoded into arrow arrays) to tag [`Value`]s.
//!
//! | parquet type | Java | Rust [`Value`] |
//! |---|---|---|
//! | `FLOAT`, `DOUBLE` | `Double` | [`Value::Double`] |
//! | `INT32`, `INT64` (signed/unsigned int annotations) | `Integer`/`Long` | [`Value::Int`] |
//! | `BOOLEAN` | `Boolean` | [`Value::Bool`] |
//! | `DECIMAL` | `Double` | [`Value::Double`] |
//! | `STRING`, `ENUM`, `JSON` | `String` | [`Value::String`] |
//! | `BINARY` | `byte[]` | [`Value::Bytes`] |
//! | `DATE` | `LocalDate` | `"2000-01-01"` string (`LocalDate.toString()`) |
//! | `TIME` | `LocalTime` | `"05:30:10.123"` string (`LocalTime.toString()`) |
//! | `TIMESTAMP` (truncated to millis), `INT96` | `Instant` | `"2000-01-01T10:30:10.123Z"` string (`Instant.toString()`) |
//! | `UUID` | `UUID` | `"606362d9-012a-4949-b91a-1ab439951671"` string |
//! | `INTERVAL` | `Interval` | [`Interval`] `toString()` string |
//!
//! [`Value`] has no date/time variants, so temporal values become their Java `toString()` representation, which is
//! also how Java encodes them into vector tile attributes. `Struct::as_timestamp` parses the timestamp strings.

use arrow::array::{Array, AsArray};
use arrow::datatypes::{
    DataType, Date32Type, Date64Type, Decimal128Type, Decimal256Type, Float16Type, Float32Type, Float64Type, Int8Type,
    Int16Type, Int32Type, Int64Type, IntervalDayTimeType, IntervalMonthDayNanoType, IntervalUnit,
    IntervalYearMonthType, Time32MillisecondType, Time32SecondType, Time64MicrosecondType, Time64NanosecondType,
    TimeUnit, TimestampMicrosecondType, TimestampMillisecondType, TimestampNanosecondType, TimestampSecondType,
    UInt8Type, UInt16Type, UInt32Type, UInt64Type,
};

use crate::reader::parquet::interval::Interval;
use crate::value::Value;

/// Extra information from the parquet schema that the arrow type does not capture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PrimitiveHint {
    #[default]
    None,
    /// `FIXED_LEN_BYTE_ARRAY(16)` annotated with `UUID`
    Uuid,
    /// `INT96` timestamp (keeps nanosecond precision like Java)
    Int96,
}

/// Java `Instant.toString()` for epoch nanoseconds.
pub fn instant_string(epoch_nanos: i128) -> String {
    let seconds = epoch_nanos.div_euclid(1_000_000_000) as i64;
    let nanos = epoch_nanos.rem_euclid(1_000_000_000) as u32;
    match chrono::DateTime::from_timestamp(seconds, nanos) {
        Some(dt) => dt.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
        None => format!("{seconds}s"),
    }
}

/// Java `Instant.toString()` of `Instant.ofEpochMilli(...)`.
pub fn instant_string_millis(epoch_millis: i64) -> String {
    instant_string(epoch_millis as i128 * 1_000_000)
}

/// Java `LocalDate.toString()` for days since the epoch.
pub fn date_string(epoch_days: i64) -> String {
    match chrono::NaiveDate::from_ymd_opt(1970, 1, 1)
        .and_then(|d| d.checked_add_signed(chrono::Duration::days(epoch_days)))
    {
        Some(d) => d.format("%Y-%m-%d").to_string(),
        None => epoch_days.to_string(),
    }
}

/// Java `LocalTime.toString()` for nanoseconds since midnight.
pub fn time_string(nano_of_day: i64) -> String {
    let nano_of_day = nano_of_day.rem_euclid(86_400_000_000_000);
    let hours = nano_of_day / 3_600_000_000_000;
    let minutes = (nano_of_day / 60_000_000_000) % 60;
    let seconds = (nano_of_day / 1_000_000_000) % 60;
    let nanos = nano_of_day % 1_000_000_000;
    let mut s = format!("{hours:02}:{minutes:02}");
    if seconds > 0 || nanos > 0 {
        s.push_str(&format!(":{seconds:02}"));
        if nanos > 0 {
            if nanos % 1_000_000 == 0 {
                s.push_str(&format!(".{:03}", nanos / 1_000_000));
            } else if nanos % 1_000 == 0 {
                s.push_str(&format!(".{:06}", nanos / 1_000));
            } else {
                s.push_str(&format!(".{nanos:09}"));
            }
        }
    }
    s
}

/// Java `UUID.toString()` for 16 big-endian bytes.
pub fn uuid_string(bytes: &[u8]) -> String {
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    if hex.len() != 32 {
        return hex;
    }
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

/// Correctly-rounded conversion of an unscaled decimal to a double (Java `BigDecimal.doubleValue()`).
pub fn decimal_to_f64(unscaled: impl std::fmt::Display, scale: i8) -> f64 {
    format!("{unscaled}e{}", -(scale as i32)).parse().unwrap_or(f64::NAN)
}

fn millis(value: i64, unit: TimeUnit) -> i64 {
    match unit {
        TimeUnit::Second => value.saturating_mul(1000),
        TimeUnit::Millisecond => value,
        TimeUnit::Microsecond => value.div_euclid(1_000),
        TimeUnit::Nanosecond => value.div_euclid(1_000_000),
    }
}

fn nanos(value: i64, unit: TimeUnit) -> i128 {
    let v = value as i128;
    match unit {
        TimeUnit::Second => v * 1_000_000_000,
        TimeUnit::Millisecond => v * 1_000_000,
        TimeUnit::Microsecond => v * 1_000,
        TimeUnit::Nanosecond => v,
    }
}

/// Converts the primitive value at `row` of `array` (which must not be null there) to a [`Value`]. Returns `None`
/// for non-primitive types.
pub fn primitive_value(array: &dyn Array, row: usize, hint: PrimitiveHint) -> Option<Value> {
    Some(match array.data_type() {
        DataType::Null => Value::Null,
        DataType::Boolean => Value::Bool(array.as_boolean().value(row)),
        DataType::Int8 => Value::Int(array.as_primitive::<Int8Type>().value(row) as i64),
        DataType::Int16 => Value::Int(array.as_primitive::<Int16Type>().value(row) as i64),
        DataType::Int32 => Value::Int(array.as_primitive::<Int32Type>().value(row) as i64),
        DataType::Int64 => Value::Int(array.as_primitive::<Int64Type>().value(row)),
        DataType::UInt8 => Value::Int(array.as_primitive::<UInt8Type>().value(row) as i64),
        DataType::UInt16 => Value::Int(array.as_primitive::<UInt16Type>().value(row) as i64),
        DataType::UInt32 => Value::Int(array.as_primitive::<UInt32Type>().value(row) as i64),
        // Java stores the raw signed long
        DataType::UInt64 => Value::Int(array.as_primitive::<UInt64Type>().value(row) as i64),
        DataType::Float16 => Value::Double(array.as_primitive::<Float16Type>().value(row).to_f64()),
        // Java converts floats to doubles
        DataType::Float32 => Value::Double(array.as_primitive::<Float32Type>().value(row) as f64),
        DataType::Float64 => Value::Double(array.as_primitive::<Float64Type>().value(row)),
        DataType::Decimal128(_, scale) => {
            Value::Double(decimal_to_f64(array.as_primitive::<Decimal128Type>().value(row), *scale))
        }
        DataType::Decimal256(_, scale) => {
            Value::Double(decimal_to_f64(array.as_primitive::<Decimal256Type>().value(row), *scale))
        }
        DataType::Utf8 => Value::str(array.as_string::<i32>().value(row)),
        DataType::LargeUtf8 => Value::str(array.as_string::<i64>().value(row)),
        DataType::Utf8View => Value::str(array.as_string_view().value(row)),
        DataType::Binary => Value::Bytes(array.as_binary::<i32>().value(row).into()),
        DataType::LargeBinary => Value::Bytes(array.as_binary::<i64>().value(row).into()),
        DataType::BinaryView => Value::Bytes(array.as_binary_view().value(row).into()),
        DataType::FixedSizeBinary(_) => {
            let bytes = array.as_fixed_size_binary().value(row);
            match hint {
                PrimitiveHint::Uuid if bytes.len() == 16 => Value::str(uuid_string(bytes)),
                _ => Value::Bytes(bytes.into()),
            }
        }
        DataType::Date32 => Value::str(date_string(array.as_primitive::<Date32Type>().value(row) as i64)),
        DataType::Date64 => {
            Value::str(date_string(array.as_primitive::<Date64Type>().value(row).div_euclid(86_400_000)))
        }
        DataType::Time32(TimeUnit::Second) => {
            Value::str(time_string(array.as_primitive::<Time32SecondType>().value(row) as i64 * 1_000_000_000))
        }
        DataType::Time32(_) => {
            Value::str(time_string(array.as_primitive::<Time32MillisecondType>().value(row) as i64 * 1_000_000))
        }
        DataType::Time64(TimeUnit::Microsecond) => {
            Value::str(time_string(array.as_primitive::<Time64MicrosecondType>().value(row) * 1_000))
        }
        DataType::Time64(_) => Value::str(time_string(array.as_primitive::<Time64NanosecondType>().value(row))),
        DataType::Timestamp(unit, _) => {
            let value = match unit {
                TimeUnit::Second => array.as_primitive::<TimestampSecondType>().value(row),
                TimeUnit::Millisecond => array.as_primitive::<TimestampMillisecondType>().value(row),
                TimeUnit::Microsecond => array.as_primitive::<TimestampMicrosecondType>().value(row),
                TimeUnit::Nanosecond => array.as_primitive::<TimestampNanosecondType>().value(row),
            };
            if hint == PrimitiveHint::Int96 {
                Value::str(instant_string(nanos(value, *unit)))
            } else {
                Value::str(instant_string_millis(millis(value, *unit)))
            }
        }
        DataType::Interval(IntervalUnit::YearMonth) => {
            Value::str(Interval::of(array.as_primitive::<IntervalYearMonthType>().value(row), 0, 0).to_string())
        }
        DataType::Interval(IntervalUnit::DayTime) => {
            let v = array.as_primitive::<IntervalDayTimeType>().value(row);
            Value::str(Interval::of(0, v.days as i64, v.milliseconds as i64).to_string())
        }
        DataType::Interval(IntervalUnit::MonthDayNano) => {
            let v = array.as_primitive::<IntervalMonthDayNanoType>().value(row);
            Value::str(Interval::of(v.months, v.days as i64, v.nanoseconds.div_euclid(1_000_000)).to_string())
        }
        DataType::Duration(_) => {
            let v = arrow::compute::cast(&array.slice(row, 1), &DataType::Int64).ok()?;
            Value::Int(v.as_primitive::<Int64Type>().value(0))
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        assert_eq!("2000-01-01", date_string(10957));
        assert_eq!("1970-01-03", date_string(2));
        assert_eq!("05:30:10.123", time_string(19_810_123_000_000));
        assert_eq!("05:30", time_string(19_800_000_000_000));
        assert_eq!("00:00:00.000001", time_string(1_000));
        assert_eq!("2000-01-01T10:30:10.123Z", instant_string_millis(946_722_610_123));
        assert_eq!("1970-01-01T00:00:00Z", instant_string_millis(0));
        assert_eq!("-4713-11-24T00:00:00Z", instant_string(-210_866_803_200_000_000_000));
        assert_eq!(
            "606362d9-012a-4949-b91a-1ab439951671",
            uuid_string(&[
                0x60, 0x63, 0x62, 0xd9, 0x01, 0x2a, 0x49, 0x49, 0xb9, 0x1a, 0x1a, 0xb4, 0x39, 0x95, 0x16, 0x71
            ])
        );
        assert_eq!(123456.789, decimal_to_f64(123456789i64, 3));
        assert_eq!(0.1, decimal_to_f64(10, 2));
        assert_eq!(10.0, decimal_to_f64(10, 0));
    }
}

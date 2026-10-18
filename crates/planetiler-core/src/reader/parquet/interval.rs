//! Port of `reader/parquet/Interval.java`: a
//! [parquet interval](https://github.com/apache/parquet-format/blob/master/LogicalTypes.md#interval) which has a month,
//! day, and millisecond part.
//!
//! Java's `Interval` is a `TemporalAmount` (`Period` + `Duration`); here it is a plain struct. When converted to a tag
//! [`Value`](crate::value::Value) it becomes its Java `toString()` (`Interval[period=P1M2D, duration=PT3S]`).

use std::fmt;

/// A period of months and days plus a duration in milliseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Interval {
    pub months: i32,
    pub days: i64,
    pub millis: i64,
}

impl Interval {
    pub fn of(months: i32, days: i64, millis: i64) -> Interval {
        Interval { months, days, millis }
    }

    /// Decode the 12-byte little-endian parquet INTERVAL representation (unsigned months, days, millis).
    pub fn from_parquet_bytes(bytes: &[u8]) -> Option<Interval> {
        if bytes.len() != 12 {
            return None;
        }
        let read = |i: usize| i32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
        Some(Interval::of(read(0), read(4) as i64, read(8) as i64))
    }

    /// Java `Period.toString()` (ISO-8601, e.g. `P1M2D`, or `P0D` for zero).
    pub fn period_string(&self) -> String {
        if self.months == 0 && self.days == 0 {
            return "P0D".to_string();
        }
        // Period.ofMonths(n).plusDays(d) keeps everything in months and days
        let mut s = String::from("P");
        if self.months != 0 {
            s.push_str(&format!("{}M", self.months));
        }
        if self.days != 0 {
            s.push_str(&format!("{}D", self.days));
        }
        s
    }

    /// Java `Duration.toString()` (ISO-8601, e.g. `PT3S`, `PT1H2M0.5S`, or `PT0S` for zero).
    pub fn duration_string(&self) -> String {
        if self.millis == 0 {
            return "PT0S".to_string();
        }
        let total_seconds = self.millis.div_euclid(1000);
        let millis = self.millis.rem_euclid(1000);
        let hours = total_seconds / 3600;
        let minutes = (total_seconds % 3600) / 60;
        let secs = total_seconds % 60;
        let mut s = String::from("PT");
        if hours != 0 {
            s.push_str(&format!("{hours}H"));
        }
        if minutes != 0 {
            s.push_str(&format!("{minutes}M"));
        }
        if secs == 0 && millis == 0 && s.len() > 2 {
            return s;
        }
        if secs < 0 && millis > 0 {
            if secs == -1 {
                s.push_str("-0");
            } else {
                s.push_str(&format!("{}", secs + 1));
            }
        } else {
            s.push_str(&format!("{secs}"));
        }
        if millis > 0 {
            let frac = if secs < 0 { 1000 - millis } else { millis };
            let frac = format!("{frac:03}");
            s.push('.');
            s.push_str(frac.trim_end_matches('0'));
        }
        s.push('S');
        s
    }
}

impl fmt::Display for Interval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Interval[period={}, duration={}]", self.period_string(), self.duration_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_string() {
        assert_eq!("Interval[period=P1M2D, duration=PT3S]", Interval::of(1, 2, 3_000).to_string());
        assert_eq!("Interval[period=P0D, duration=PT0S]", Interval::of(0, 0, 0).to_string());
        assert_eq!("Interval[period=P14M, duration=PT1H1M1.5S]", Interval::of(14, 0, 3_661_500).to_string());
        assert_eq!("PT0.123S", Interval::of(0, 0, 123).duration_string());
    }

    #[test]
    fn from_bytes() {
        let mut bytes = vec![];
        bytes.extend_from_slice(&1i32.to_le_bytes());
        bytes.extend_from_slice(&2i32.to_le_bytes());
        bytes.extend_from_slice(&3000i32.to_le_bytes());
        assert_eq!(Some(Interval::of(1, 2, 3000)), Interval::from_parquet_bytes(&bytes));
        assert_eq!(None, Interval::from_parquet_bytes(&bytes[..11]));
    }
}

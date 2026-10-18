//! Port of `util/SortKey.java`: computes integer sort keys for features where the integer key ordering approximates
//! the desired ordering.
//!
//! Sort keys get packed into [`SORT_KEY_BITS`] bits, so sort key components need to specify the range and number of
//! levels the range gets packed into. Requests that exceed the total number of available levels will panic.
//!
//! To sort by a field descending, specify its range from high to low.
//!
//! For example this SQL ordering:
//!
//! ```sql
//! ORDER BY rank ASC, population DESC, length(name) ASC
//! ```
//!
//! would become:
//!
//! ```ignore
//! feature.set_sort_key(
//!     SortKey::order_by_int(rank, MIN_RANK, MAX_RANK)
//!         .then_by_log(population, MAX_POPULATION, MIN_POPULATION, NUM_LEVELS)
//!         .then_by_int(name.len() as i32, 0, MAX_LENGTH)
//!         .get(),
//! );
//! ```

/// Number of bits available for sort keys (mirrors `FeatureGroup.SORT_KEY_BITS`).
pub const SORT_KEY_BITS: u32 = 22;
/// Maximum sort key value (mirrors `FeatureGroup.SORT_KEY_MAX`).
pub const SORT_KEY_MAX: i32 = (1 << (SORT_KEY_BITS - 1)) - 1;
/// Minimum sort key value (mirrors `FeatureGroup.SORT_KEY_MIN`).
pub const SORT_KEY_MIN: i32 = -(1 << (SORT_KEY_BITS - 1));

const MAX: i64 = SORT_KEY_MAX as i64 - SORT_KEY_MIN as i64;

/// Builder for a sort key made up of several components.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[must_use]
pub struct SortKey {
    possible_values: i64,
    result: i64,
}

impl SortKey {
    fn new() -> SortKey {
        SortKey { possible_values: 1, result: 0 }
    }

    /// Returns a new sort key where elements with `value == true` sort after ones where `value == false`.
    pub fn order_by_trues_last(value: bool) -> SortKey {
        Self::new().then_by_trues_last(value)
    }

    /// Returns a new sort key where elements with `value == true` sort before ones where `value == false`.
    pub fn order_by_trues_first(value: bool) -> SortKey {
        Self::new().then_by_trues_first(value)
    }

    /// Returns a new sort key where elements with `value == start` appear before elements with `value == end`.
    pub fn order_by_int(value: i32, start: i32, end: i32) -> SortKey {
        Self::new().then_by_int(value, start, end)
    }

    /// Returns a new sort key where elements with `value == start` appear before elements with `value == end` and the
    /// entire range is compressed into `levels` distinct levels for comparison.
    pub fn order_by_double(value: f64, start: f64, end: f64, levels: i32) -> SortKey {
        Self::new().then_by_double(value, start, end, levels)
    }

    /// Returns a new sort key where elements with `value == start` appear before elements with `value == end` and the
    /// log of the entire range is compressed into the maximum allowed number of distinct levels for comparison.
    pub fn order_by_log_max_levels(value: f64, start: f64, end: f64) -> SortKey {
        Self::new().then_by_log(value, start, end, MAX as i32)
    }

    /// Returns a new sort key where elements with `value == start` appear before elements with `value == end` and the
    /// log of the entire range is compressed into `levels` distinct levels for comparison.
    pub fn order_by_log(value: f64, start: f64, end: f64, levels: i32) -> SortKey {
        Self::new().then_by_log(value, start, end, levels)
    }

    fn double_range_to_int(value: f64, min: f64, max: f64, levels: i32) -> i32 {
        ((levels - 1) as f64).min(((value - min) / (max - min) * levels as f64).floor()) as i32
    }

    /// Returns an integer sort key reflecting the ordering of sort fields added so far.
    pub fn get(&self) -> i32 {
        (self.result + SORT_KEY_MIN as i64) as i32
    }

    /// Adds a field where if all previous values are equal, elements where `value == true` sort before `false`.
    pub fn then_by_trues_first(self, value: bool) -> SortKey {
        self.then_by_int(if value { 0 } else { 1 }, 0, 1)
    }

    /// Adds a field where if all previous values are equal, elements where `value == true` sort after `false`.
    pub fn then_by_trues_last(self, value: bool) -> SortKey {
        self.then_by_int(if value { 1 } else { 0 }, 0, 1)
    }

    /// Adds a field where if all previous values are equal, elements where `value == start` sort before elements where
    /// `value == end`.
    pub fn then_by_int(self, value: i32, start: i32, end: i32) -> SortKey {
        if start > end {
            return self.then_by_int(start - value, end, start);
        }
        let levels = end as i64 + 1 - start as i64;
        let value = value.clamp(start, end);
        self.accumulate(value as i64, start as i64, levels)
    }

    /// Adds a field where if all previous values are equal, elements where `value == start` sort before elements where
    /// `value == end` and the range is broken into `levels` discrete levels.
    pub fn then_by_double(self, value: f64, start: f64, end: f64, levels: i32) -> SortKey {
        assert!(levels > 0, "levels must be > 0 got {levels}");
        if start > end {
            return self.then_by_double(start - value, end, start, levels);
        }
        let value = if value < start || value > end { value.clamp(start, end) } else { value };
        let int_val = Self::double_range_to_int(value, start, end, levels);
        self.accumulate(int_val as i64, 0, levels as i64)
    }

    /// Adds a field where if all previous values are equal, elements where `value == start` sort before elements where
    /// `value == end` and the range is broken into `levels` discrete levels based on the log of the input.
    pub fn then_by_log(self, value: f64, start: f64, end: f64, levels: i32) -> SortKey {
        assert!(levels > 0, "levels must be > 0 got {levels}");
        if start > end {
            return self.then_by_log(start / value, end, start, levels);
        }
        assert!(start > 0.0, "log thresholds must be > 0 got [{start}, {end}]");
        let value = if value < start || value > end { value.clamp(start, end) } else { value };
        let int_val = Self::double_range_to_int(value.ln(), start.ln(), end.ln(), levels);
        self.accumulate(int_val as i64, 0, levels as i64)
    }

    fn accumulate(mut self, value: i64, min: i64, levels: i64) -> SortKey {
        self.possible_values *= levels;
        if self.possible_values > MAX {
            panic!("Too many possible values: {}", self.possible_values);
        }
        self.result = self.result * levels + value - min;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::catch_unwind;

    fn assert_less_than(a: i32, b: i32) {
        assert!(a < b, "{a} was not < {b}");
    }

    #[test]
    fn test_single_level() {
        assert_less_than(SortKey::order_by_int(1, 0, 10).get(), SortKey::order_by_int(2, 0, 10).get());
        assert_eq!(SortKey::order_by_int(1, 0, 10).get(), SortKey::order_by_int(1, 0, 10).get());
        assert_eq!(SortKey::order_by_int(-1, 0, 10).get(), SortKey::order_by_int(-2, 0, 10).get());
        assert_eq!(SortKey::order_by_int(11, 0, 10).get(), SortKey::order_by_int(12, 0, 10).get());
    }

    #[test]
    fn test_two_level() {
        let key = |a, b| SortKey::order_by_int(a, 0, 10).then_by_int(b, 0, 10).get();
        assert_eq!(key(1, 1), key(1, 1));
        assert_less_than(key(1, 1), key(1, 2));
        assert_less_than(key(1, 1), key(2, 1));
    }

    #[test]
    fn test_descending() {
        let key = |a, b| SortKey::order_by_int(a, 0, 10).then_by_int(b, 10, 0).get();
        assert_eq!(key(0, 1), key(0, 1));
        assert_less_than(key(10, 2), key(10, 1));
        assert_less_than(key(1, 1), key(2, 1));
    }

    #[test]
    fn test_double() {
        let key = |a, b| SortKey::order_by_double(a, 0.0, 10.0, 10).then_by_double(b, 10.0, 0.0, 10).get();
        assert_eq!(key(0.0, 0.0), key(0.9, 0.9));
        assert_less_than(key(0.9, 1.0), key(1.1, 1.0));
        assert_less_than(key(0.0, 1.1), key(0.0, 0.9));
    }

    #[test]
    fn test_boolean() {
        let key = |a, b| SortKey::order_by_trues_first(a).then_by_trues_last(b).get();
        assert_eq!(key(false, false), key(false, false));
        assert_less_than(key(true, false), key(false, false));
        assert_less_than(key(false, false), key(false, true));
        assert_less_than(SortKey::order_by_trues_last(false).get(), SortKey::order_by_trues_last(true).get());
    }

    #[test]
    fn test_log() {
        let key = |a, b| SortKey::order_by_log(a, 1.0, 1000.0, 3).then_by_log(b, 1000.0, 1.0, 3).get();
        assert_eq!(key(1.0, 1.0), key(1.0, 1.0));
        assert_eq!(key(1.0, 1.0), key(9.0, 9.0));
        assert_eq!(key(-1.0, 0.0), key(0.0, 0.0));
        assert_eq!(key(1_000.0, 0.0), key(10_000.0, 0.0));
        assert_eq!(key(11.0, 11.0), key(99.0, 99.0));
        assert_less_than(key(9.0, 1.0), key(11.0, 1.0));
        assert_less_than(key(99.0, 1.0), key(101.0, 1.0));
        assert_less_than(key(1.0, 11.0), key(1.0, 9.0));
        assert_less_than(key(1.0, 101.0), key(1.0, 99.0));

        assert!(catch_unwind(|| SortKey::order_by_log(1.0, 0.0, 2.0, 2)).is_err());
        assert!(catch_unwind(|| SortKey::order_by_log(1.0, 1.0, 0.0, 2)).is_err());
    }

    #[test]
    fn test_too_much_resolution() {
        let max = (1 << SORT_KEY_BITS) - 1;
        let below_half_max = (1 << (SORT_KEY_BITS / 2)) - 1;
        let above_half_max = below_half_max * 2;

        let _ = SortKey::order_by_double(0.0, 1.0, 0.0, max);
        assert!(catch_unwind(|| SortKey::order_by_double(0.0, 1.0, 0.0, max + 1)).is_err());
        let _ = SortKey::order_by_double(0.0, 1.0, 0.0, below_half_max - 1).then_by_double(
            0.0,
            1.0,
            0.0,
            below_half_max - 1,
        );
        assert!(
            catch_unwind(|| SortKey::order_by_double(0.0, 1.0, 0.0, above_half_max).then_by_double(
                0.0,
                1.0,
                0.0,
                above_half_max
            ))
            .is_err()
        );
    }
}

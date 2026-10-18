//! Port of `util/ZoomFunction.java`: a value that changes by zoom level.
//!
//! [`ZoomFunction::apply`] returns the value at a zoom level, or `None`. Java's `ZoomFunction<Number>` maps to
//! `ZoomFunction<f64>`; use [`apply_as_double_or_else`] / [`apply_as_int_or_else`] / [`apply_or_else`] to evaluate an
//! optional function with a fallback.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use crate::geo::geo_utils;

/// A cheaply-cloneable, thread-safe function from zoom level to an optional value.
pub struct ZoomFunction<T>(Arc<dyn Fn(u32) -> Option<T> + Send + Sync>);

impl<T> Clone for ZoomFunction<T> {
    fn clone(&self) -> Self {
        ZoomFunction(self.0.clone())
    }
}

impl<T> fmt::Debug for ZoomFunction<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ZoomFunction@{:p}", Arc::as_ptr(&self.0))
    }
}

impl<T: 'static> ZoomFunction<T> {
    /// Wraps an arbitrary function of zoom level.
    pub fn new(f: impl Fn(u32) -> Option<T> + Send + Sync + 'static) -> ZoomFunction<T> {
        ZoomFunction(Arc::new(f))
    }

    /// Returns the value at `zoom`.
    pub fn apply(&self, zoom: u32) -> Option<T> {
        (self.0)(zoom)
    }
}

impl<T: Clone + Send + Sync + 'static> ZoomFunction<T> {
    /// Returns `value` when `zoom >= min`, and `None` otherwise.
    pub fn min_zoom(min: u32, value: T) -> ZoomFunction<T> {
        Self::new(move |zoom| (zoom >= min).then(|| value.clone()))
    }

    /// Returns `value` when zoom is between `min` and `max` inclusive, `None` otherwise.
    pub fn zoom_range(min: u32, max: u32, value: T) -> ZoomFunction<T> {
        Self::new(move |zoom| (zoom >= min && zoom <= max).then(|| value.clone()))
    }

    /// Returns `value` when `zoom <= max`, and `None` otherwise.
    pub fn max_zoom(max: u32, value: T) -> ZoomFunction<T> {
        Self::new(move |zoom| (zoom <= max).then(|| value.clone()))
    }

    /// Returns a zoom function that returns the value from the next higher key in `thresholds` or `None` if over the
    /// max key.
    pub fn from_max_zoom_thresholds(thresholds: impl IntoIterator<Item = (u32, T)>) -> ZoomFunction<T> {
        Self::from_max_zoom_thresholds_or(thresholds, None)
    }

    /// Returns a zoom function that returns the value from the next higher key in `thresholds` or `default_value`.
    pub fn from_max_zoom_thresholds_or(
        thresholds: impl IntoIterator<Item = (u32, T)>,
        default_value: Option<T>,
    ) -> ZoomFunction<T> {
        let ordered: BTreeMap<u32, T> = thresholds.into_iter().collect();
        Self::new(move |zoom| ordered.range(zoom..).next().map(|(_, v)| v.clone()).or_else(|| default_value.clone()))
    }
}

/// Numeric types that can be converted like Java's `Number.doubleValue()` / `intValue()`.
pub trait ZoomNumber {
    fn to_f64(&self) -> f64;
    fn to_i32(&self) -> i32 {
        self.to_f64() as i32
    }
}

macro_rules! zoom_number {
    ($($t:ty),*) => {
        $(impl ZoomNumber for $t {
            fn to_f64(&self) -> f64 {
                *self as f64
            }
            fn to_i32(&self) -> i32 {
                *self as i32
            }
        })*
    };
}
zoom_number!(i32, u32, i64, u64, f32, f64, usize);

/// Invoke a function at a zoom level and returns `default_value` if the function or result were missing.
pub fn apply_as_double_or_else<T: ZoomNumber + 'static>(
    f: Option<&ZoomFunction<T>>,
    zoom: u32,
    default_value: f64,
) -> f64 {
    f.and_then(|f| f.apply(zoom)).map_or(default_value, |v| v.to_f64())
}

/// Invoke a function at a zoom level and returns `default_value` if the function or result were missing.
pub fn apply_as_int_or_else<T: ZoomNumber + 'static>(
    f: Option<&ZoomFunction<T>>,
    zoom: u32,
    default_value: i32,
) -> i32 {
    f.and_then(|f| f.apply(zoom)).map_or(default_value, |v| v.to_i32())
}

/// Invoke a function at a zoom level and returns `default_value` if the function or result were missing.
pub fn apply_or_else<T: 'static>(f: Option<&ZoomFunction<T>>, zoom: u32, default_value: T) -> T {
    f.and_then(|f| f.apply(zoom)).unwrap_or(default_value)
}

/// A zoom function that lets you set the value to return for a zoom level in meters and when called, it returns how
/// many pixels long that number of meters is at the equator.
#[derive(Clone, Debug, Default)]
pub struct MeterToPixelThresholds {
    levels: BTreeMap<u32, f64>,
}

impl MeterToPixelThresholds {
    /// Sets the value to return at `zoom` in meters.
    pub fn put(mut self, zoom: u32, meters: f64) -> Self {
        self.levels.insert(zoom, geo_utils::meters_to_pixel_at_equator(zoom, meters));
        self
    }

    /// Returns the pixel value at `zoom`, if set.
    pub fn apply(&self, zoom: u32) -> Option<f64> {
        self.levels.get(&zoom).copied()
    }

    /// Converts this into a [`ZoomFunction`].
    pub fn into_fn(self) -> ZoomFunction<f64> {
        ZoomFunction::new(move |zoom| self.apply(zoom))
    }
}

impl From<MeterToPixelThresholds> for ZoomFunction<f64> {
    fn from(value: MeterToPixelThresholds) -> Self {
        value.into_fn()
    }
}

/// Returns a new builder for a zoom function from meter thresholds.
pub fn meter_thresholds() -> MeterToPixelThresholds {
    MeterToPixelThresholds::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_value_in_range<T: PartialEq + fmt::Debug + Clone + 'static>(
        f: &ZoomFunction<T>,
        min: u32,
        max: u32,
        value: Option<T>,
    ) {
        for i in min..=max {
            assert_eq!(value, f.apply(i), "z{i}");
        }
    }

    #[test]
    fn test_null_below_zoom() {
        let f = ZoomFunction::min_zoom(10, "value");
        assert_value_in_range(&f, 0, 9, None);
        assert_value_in_range(&f, 10, 14, Some("value"));
    }

    #[test]
    fn test_null_above_zoom() {
        let f = ZoomFunction::max_zoom(10, "value");
        assert_value_in_range(&f, 0, 10, Some("value"));
        assert_value_in_range(&f, 11, 14, None);
    }

    #[test]
    fn test_value_in_range() {
        let f = ZoomFunction::zoom_range(10, 12, "value");
        assert_value_in_range(&f, 0, 9, None);
        assert_value_in_range(&f, 10, 12, Some("value"));
        assert_value_in_range(&f, 13, 14, None);
    }

    #[test]
    fn test_multiple_thresholds() {
        let f = ZoomFunction::from_max_zoom_thresholds([(3, "3"), (5, "5")]);
        assert_value_in_range(&f, 0, 3, Some("3"));
        assert_value_in_range(&f, 4, 5, Some("5"));
        assert_value_in_range(&f, 6, 14, None);
        let f = ZoomFunction::from_max_zoom_thresholds_or([(3, "3")], Some("d"));
        assert_eq!(Some("d"), f.apply(4));
    }

    #[test]
    fn test_convert_meters_to_pixels() {
        let f = meter_thresholds().put(7, 20_000.0).put(8, 14_000.0).put(11, 8_000.0);
        assert_eq!(None, f.apply(6));
        assert!((16.0 - f.apply(7).unwrap()).abs() < 1.0);
        assert!((23.0 - f.apply(8).unwrap()).abs() < 1.0);
        assert_eq!(None, f.apply(9));
        assert_eq!(None, f.apply(10));
        assert!((105.0 - f.apply(11).unwrap()).abs() < 1.0);
        assert_eq!(None, f.apply(12));
        let f: ZoomFunction<f64> = f.into();
        assert_eq!(None, f.apply(6));
    }

    #[test]
    fn test_apply_or_else() {
        let f = ZoomFunction::max_zoom(5, 2);
        assert_eq!(2.0, apply_as_double_or_else(Some(&f), 3, 1.0));
        assert_eq!(1.0, apply_as_double_or_else(Some(&f), 6, 1.0));
        assert_eq!(1.0, apply_as_double_or_else::<i32>(None, 3, 1.0));
        assert_eq!(2, apply_as_int_or_else(Some(&f), 3, 1));
        assert_eq!(7, apply_or_else(Some(&f), 6, 7));
    }
}

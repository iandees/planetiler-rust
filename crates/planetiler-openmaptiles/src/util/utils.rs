//! Port of `util/Utils.java`: common utilities for working with data and the OpenMapTiles schema in
//! [`layers`](crate::layers) implementations.
//!
//! Java's `coalesce(a, b, ...)` is `a.or(b).or(...)` on `Option`s in Rust.

use planetiler_core::util::parse;
use planetiler_core::value::{Tags, Value};

/// Returns `a`, or `None` if `a` is `null_value`.
#[inline]
pub fn null_if_long(a: i64, null_value: i64) -> Option<i64> {
    if a == null_value { None } else { Some(a) }
}

/// Returns `a`, or `None` if `a` is `null_value`.
#[inline]
pub fn null_if_int(a: i32, null_value: i32) -> Option<i32> {
    if a == null_value { None } else { Some(a) }
}

/// Returns `a`, or `None` if `a` is `""`.
#[inline]
pub fn null_if_empty<S: AsRef<str>>(a: Option<S>) -> Option<S> {
    a.filter(|s| !s.as_ref().is_empty())
}

/// Returns true if `a` is `None` or `""`.
#[inline]
pub fn null_or_empty<S: AsRef<str>>(a: Option<S>) -> bool {
    a.is_none_or(|s| s.as_ref().is_empty())
}

/// Returns true if `a` is missing, null, or its string value is `""`.
#[inline]
pub fn null_or_empty_value(a: Option<&Value>) -> bool {
    match a {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        Some(_) => false,
    }
}

/// Returns `(ele, ele_ft)` attributes from an elevation in meters.
#[inline]
pub fn elevation(meters: f64) -> (i32, i32) {
    (java_round(meters), java_round(meters * 3.2808399))
}

/// Java `(int) Math.round(d)`.
#[inline]
fn java_round(d: f64) -> i32 {
    (d + 0.5).floor() as i32
}

/// Returns a map with `ele` (meters) and `ele_ft` attributes from an elevation in meters.
pub fn elevation_tags(meters: f64) -> Tags {
    let (ele, ele_ft) = elevation(meters);
    let mut result = Tags::with_capacity(2);
    result.insert("ele".into(), ele.into());
    result.insert("ele_ft".into(), ele_ft.into());
    result
}

/// Returns a map with `ele` (meters) and `ele_ft` attributes from an elevation string in meters, if `meters` can be
/// parsed as a valid number.
pub fn elevation_tags_str(meters: Option<&str>) -> Tags {
    match meters.and_then(parse::meters_str) {
        Some(ele) => elevation_tags(ele),
        None => Tags::new(),
    }
}

/// Returns "bridge" or "tunnel" string used for "brunnel" attribute by OpenMapTiles schema.
#[inline]
pub fn brunnel(is_bridge: bool, is_tunnel: bool) -> Option<&'static str> {
    brunnel_with_ford(is_bridge, is_tunnel, false)
}

/// Returns "bridge" or "tunnel" or "ford" string used for "brunnel" attribute by OpenMapTiles schema.
#[inline]
pub fn brunnel_with_ford(is_bridge: bool, is_tunnel: bool, is_ford: bool) -> Option<&'static str> {
    if is_bridge {
        Some("bridge")
    } else if is_tunnel {
        Some("tunnel")
    } else if is_ford {
        Some("ford")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_elevation() {
        assert_eq!((100, 328), elevation(100.0));
        assert_eq!((-2, -8), elevation(-2.4));
        assert!(elevation_tags_str(Some("abc")).is_empty());
        assert_eq!(Some(&Value::Int(100)), elevation_tags_str(Some("100")).get("ele"));
    }

    #[test]
    fn test_brunnel() {
        assert_eq!(Some("bridge"), brunnel(true, true));
        assert_eq!(Some("tunnel"), brunnel(false, true));
        assert_eq!(None, brunnel(false, false));
        assert_eq!(Some("ford"), brunnel_with_ford(false, false, true));
    }

    #[test]
    fn test_null_helpers() {
        assert_eq!(None, null_if_empty(Some("")));
        assert_eq!(Some("a"), null_if_empty(Some("a")));
        assert!(null_or_empty(None::<&str>));
        assert!(null_or_empty(Some("")));
        assert!(!null_or_empty(Some("a")));
        assert_eq!(None, null_if_long(0, 0));
        assert_eq!(Some(1), null_if_int(1, 0));
    }
}

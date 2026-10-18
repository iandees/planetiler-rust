//! Port of the JTS buffer operation (`org.locationtech.jts.operation.buffer`): `BufferOp`, `BufferBuilder`,
//! `BufferCurveSetBuilder`, `OffsetCurveBuilder`, `OffsetSegmentGenerator`, `OffsetSegmentString`,
//! `BufferInputLineSimplifier` and `BufferParameters`.
//!
//! The raw offset curves are generated exactly like JTS (same floating point operations, so the same vertices).
//! JTS then nodes the curves, builds a planar graph, computes the depth of every edge and keeps the edges with a
//! depth of at least 1 on one side and at most 0 on the other. Here the curves (oriented so their interior is on the
//! left) are noded and resolved by `i_overlay`'s integer overlay with a positive winding fill rule on a grid that
//! keeps (nearly) full double precision ([`polygon_overlay::positive_winding_region`]), which selects the same
//! region. The intersection points between curves are then recomputed the way JTS computes them
//! (`RobustLineIntersector` on the two curve segments) and each ring is rotated to start where JTS's graph would
//! start it.
//!
//! JTS's floating point noding is not robust: when the rounded intersection points leave the curves not fully noded,
//! building the result throws and `BufferOp` retries with snap-rounding at 12 significant digits. The port predicts
//! that case by emulating JTS's noding and validity check ([`noding`]) and then uses snap-rounding at the same
//! precision ([`polygon_overlay::positive_winding_region_snap_rounded`]).
//!
//! Differences from JTS:
//! - Where curves cross within a few ulps of each other (e.g. several curves through nearly the same point), JTS's
//!   floating point noding keeps clusters of nearly identical intersection points that `i_overlay` merges.
//! - Sometimes JTS's noding is invalid but it does not notice and returns a wrong (often empty) result
//!   (locationtech/jts#1183); the port uses the snap-rounding result instead.
//! - If snap-rounding at 12 digits fails JTS retries with fewer digits; the port never does.
//! - `sin`/`cos` (limited mitre and round joins) use fdlibm (`StrictMath`); Java's `Math.sin`/`Math.cos` intrinsics
//!   can differ in the last bit.
//! - Output polygons follow the [`polygon_overlay`] conventions (shells clockwise, holes counter-clockwise); polygon
//!   order can differ from JTS.

pub mod curve_set_builder;
pub mod input_line_simplifier;
pub mod noding;
pub mod offset_curve_builder;
pub mod offset_segment_generator;
pub mod parameters;

use geo::Area;
use geo_types::{Geometry, Polygon};

pub use self::parameters::{BufferParameters, CapStyle, JoinStyle};
use crate::geo::geometry_exception::GeometryResult;
use crate::geo::polygon_overlay;
use crate::geo::precision_model::PrecisionModel;

/// JTS `BufferOp.bufferOp(geom, distance, params)`: the buffer of `geom` by `distance` (negative to erode
/// polygons). The result is polygonal (an empty polygon if the buffer is empty).
pub fn buffer(geom: &Geometry<f64>, distance: f64, params: &BufferParameters) -> GeometryResult<Geometry<f64>> {
    let curves = curve_set_builder::CurveSetBuilder::new(distance, *params).curves(geom);
    // short-circuit test
    if curves.is_empty() {
        return Ok(polygon_overlay::to_polygonal(vec![]));
    }
    // fast path for the common case of a single simple curve (the region is the curve itself)
    let simple = match curves.as_slice() {
        [curve] if simple_curve_fast_path_enabled() && noding::is_simple_closed_curve(curve) => {
            polygon_overlay::simple_curve_positive_winding_region(curve)
        }
        _ => None,
    };
    let mut result = if let Some(simple) = simple {
        simple
    } else if noding::floating_noding_is_valid(&curves) {
        polygon_overlay::positive_winding_region(&curves)?
    } else {
        // JTS's floating point buffer would fail, and BufferOp.bufferReducedPrecision retries with snap-rounding
        // at 12 significant digits
        let scale = precision_scale_factor(geom, distance, MAX_PRECISION_DIGITS);
        let curves =
            curve_set_builder::CurveSetBuilder::with_precision(distance, *params, PrecisionModel::fixed(scale))
                .curves(geom);
        if curves.is_empty() {
            return Ok(polygon_overlay::to_polygonal(vec![]));
        }
        let intersections = noding::intersection_points(&curves);
        polygon_overlay::positive_winding_region_snap_rounded(&curves, &intersections, scale)?
    };
    if distance > 0.0 && num_geometries(geom) == 1 && result.len() > 1 {
        // Check to ensure that the buffer of a single geometry (with positive distance) is a single polygon (the
        // offset curve of a geometry with a narrow concave angle can produce small spurious "islands")
        result = vec![keep_largest_area(result)];
    }
    Ok(polygon_overlay::to_polygonal(result))
}

#[cfg(not(test))]
#[inline]
fn simple_curve_fast_path_enabled() -> bool {
    true
}

#[cfg(test)]
thread_local! {
    /// Tests turn the single simple curve fast path off to compare it with the general path.
    pub(crate) static SIMPLE_CURVE_FAST_PATH: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

#[cfg(test)]
fn simple_curve_fast_path_enabled() -> bool {
    SIMPLE_CURVE_FAST_PATH.with(|f| f.get())
}

/// JTS `BufferOp.MAX_PRECISION_DIGITS`.
const MAX_PRECISION_DIGITS: i32 = 12;

/// JTS `BufferOp.precisionScaleFactor`: the scale of a precision model with `max_precision_digits` significant
/// digits for the buffer of `geom` by `distance`.
fn precision_scale_factor(geom: &Geometry<f64>, distance: f64, max_precision_digits: i32) -> f64 {
    let env = crate::geo::envelope::Envelope::of_geometry(geom);
    let env_max = env.max_x.abs().max(env.max_y.abs()).max(env.min_x.abs()).max(env.min_y.abs());
    let expand_by_distance = if distance > 0.0 { distance } else { 0.0 };
    let buf_env_max = env_max + 2.0 * expand_by_distance;
    // the smallest power of 10 greater than the buffer envelope
    let buf_env_precision_digits = (libm::log(buf_env_max) / libm::log(10.0) + 1.0) as i32;
    let min_unit_log10 = max_precision_digits - buf_env_precision_digits;
    10f64.powi(min_unit_log10)
}

/// JTS `Geometry.getNumGeometries`.
fn num_geometries(geom: &Geometry<f64>) -> usize {
    match geom {
        Geometry::MultiPolygon(mp) => mp.0.len(),
        Geometry::MultiLineString(ml) => ml.0.len(),
        Geometry::MultiPoint(mp) => mp.0.len(),
        Geometry::GeometryCollection(gc) => gc.0.len(),
        _ => 1,
    }
}

fn keep_largest_area(polys: Vec<Polygon<f64>>) -> Polygon<f64> {
    let mut largest: Option<(f64, Polygon<f64>)> = None;
    for poly in polys {
        let area = poly.unsigned_area();
        if largest.as_ref().is_none_or(|(largest_area, _)| area > *largest_area) {
            largest = Some((area, poly));
        }
    }
    largest.expect("non-empty").1
}

#[cfg(test)]
mod tests;

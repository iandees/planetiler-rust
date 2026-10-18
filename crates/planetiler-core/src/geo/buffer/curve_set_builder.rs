//! Port of JTS `BufferCurveSetBuilder`: creates all the raw offset curves for a buffer of a geometry.
//!
//! Each curve is returned with the side its interior is on: JTS labels every curve with the topological location
//! (interior / exterior) of its left and right side; here curves are oriented so that the interior is always on the
//! left (counter-clockwise for a simple ring), which is what the positive winding fill rule used to node and union
//! them expects.

use geo_types::{Coord, Geometry, LineString, Polygon};

use super::offset_curve_builder::OffsetCurveBuilder;
use super::offset_segment_generator::Side;
use super::parameters::BufferParameters;
use crate::geo::cg_algorithms::{distance, is_ccw_area, is_ring, point_to_segment, point_to_segment_string};
use crate::geo::precision_model::PrecisionModel;

pub struct CurveSetBuilder {
    distance: f64,
    curve_builder: OffsetCurveBuilder,
    /// Raw curves with the interior on their left.
    curves: Vec<Vec<Coord<f64>>>,
}

/// JTS `LinearRing.MINIMUM_VALID_SIZE`.
const MINIMUM_VALID_SIZE: usize = 4;
const MAX_INVERTED_RING_SIZE: usize = 9;
const INVERTED_CURVE_VERTEX_FACTOR: usize = 4;
const NEARNESS_FACTOR: f64 = 0.99;

impl CurveSetBuilder {
    pub fn new(distance: f64, params: BufferParameters) -> CurveSetBuilder {
        Self::with_precision(distance, params, PrecisionModel::Floating)
    }

    /// A builder whose curve points are rounded to `precision`.
    pub fn with_precision(distance: f64, params: BufferParameters, precision: PrecisionModel) -> CurveSetBuilder {
        CurveSetBuilder { distance, curve_builder: OffsetCurveBuilder::new(params, precision), curves: Vec::new() }
    }

    /// Computes the set of raw offset curves for the buffer of `geom`. Each curve has its interior on its left.
    pub fn curves(mut self, geom: &Geometry<f64>) -> Vec<Vec<Coord<f64>>> {
        self.add(geom);
        self.curves
    }

    /// Adds a raw offset curve with the given locations on its left and right side (`true` = interior).
    fn add_curve(&mut self, coord: Option<Vec<Coord<f64>>>, left_interior: bool) {
        // don't add null or trivial curves
        let Some(mut coord) = coord else { return };
        if coord.len() < 2 {
            return;
        }
        if !left_interior {
            coord.reverse();
        }
        self.curves.push(coord);
    }

    fn add(&mut self, g: &Geometry<f64>) {
        match g {
            Geometry::Polygon(p) => self.add_polygon(p),
            Geometry::MultiPolygon(mp) => mp.0.iter().for_each(|p| self.add_polygon(p)),
            Geometry::LineString(l) => self.add_line_string(&l.0),
            Geometry::MultiLineString(ml) => ml.0.iter().for_each(|l| self.add_line_string(&l.0)),
            Geometry::Line(l) => self.add_line_string(&[l.start, l.end]),
            Geometry::Point(p) => self.add_point(p.0),
            Geometry::MultiPoint(mp) => mp.0.iter().for_each(|p| self.add_point(p.0)),
            Geometry::GeometryCollection(gc) => gc.0.iter().for_each(|g| self.add(g)),
            Geometry::Rect(r) => self.add_polygon(&r.to_polygon()),
            Geometry::Triangle(t) => self.add_polygon(&t.to_polygon()),
        }
    }

    fn add_point(&mut self, p: Coord<f64>) {
        // a zero or negative width buffer of a point is empty
        if self.distance <= 0.0 {
            return;
        }
        // skip if coordinate is invalid
        if !(p.x.is_finite() && p.y.is_finite()) {
            return;
        }
        let curve = self.curve_builder.get_line_curve(&[p], self.distance);
        // curves of lines and points are clockwise: exterior on the left, interior on the right
        self.add_curve(curve, false);
    }

    fn add_line_string(&mut self, line: &[Coord<f64>]) {
        if self.curve_builder.is_line_offset_empty(self.distance) {
            return;
        }
        let coord = clean(line);
        // skip if no valid coordinates
        if coord.is_empty() {
            return;
        }
        if is_ring(&coord) && !self.curve_builder.buffer_parameters().is_single_sided {
            self.add_linear_ring_sides(&coord, self.distance);
        } else {
            let curve = self.curve_builder.get_line_curve(&coord, self.distance);
            self.add_curve(curve, false);
        }
    }

    fn add_polygon(&mut self, p: &Polygon<f64>) {
        let mut offset_distance = self.distance;
        let mut offset_side = Side::Left;
        if self.distance < 0.0 {
            offset_distance = -self.distance;
            offset_side = Side::Right;
        }
        let shell = p.exterior();
        if shell.0.is_empty() {
            return;
        }
        // optimization - don't compute buffer if the polygon would be completely eroded
        if self.distance < 0.0 && is_ring_fully_eroded(&shell.0, false, self.distance) {
            return;
        }
        let shell_coords = clean(&shell.0);
        // skip if no valid coordinates
        if shell_coords.is_empty() {
            return;
        }
        // don't attempt to buffer a polygon with too few distinct vertices
        if self.distance <= 0.0 && shell_coords.len() < 3 {
            return;
        }
        // CW shell: exterior on the left, interior on the right
        self.add_polygon_ring_side(&shell_coords, offset_distance, offset_side, false);

        for hole in p.interiors() {
            let hole: &LineString<f64> = hole;
            if hole.0.is_empty() {
                continue;
            }
            // optimization - don't compute buffer for this hole if the hole would be completely covered
            if self.distance > 0.0 && is_ring_fully_eroded(&hole.0, true, self.distance) {
                continue;
            }
            let hole_coords = clean(&hole.0);
            // skip if no valid coordinates
            if hole_coords.is_empty() {
                continue;
            }
            // Holes are topologically labelled opposite to the shell, since the interior of the polygon lies on
            // their opposite side (on the left, if the hole is oriented CCW)
            self.add_polygon_ring_side(&hole_coords, offset_distance, offset_side.opposite(), true);
        }
    }

    /// Adds an offset curve for one side of a polygon ring. The side and left and right topological location
    /// arguments are provided as if the ring is oriented CW (`cw_left_interior` is the location on the left of a
    /// CW ring). If the ring is in the opposite orientation, the left and right locations are interchanged and the
    /// side is flipped.
    fn add_polygon_ring_side(
        &mut self,
        coord: &[Coord<f64>],
        offset_distance: f64,
        mut side: Side,
        cw_left_interior: bool,
    ) {
        // don't bother adding ring if it is "flat" and will disappear in the output
        if offset_distance == 0.0 && coord.len() < MINIMUM_VALID_SIZE {
            return;
        }
        let mut left_interior = cw_left_interior;
        if coord.len() >= MINIMUM_VALID_SIZE && is_ccw_area(coord) {
            left_interior = !cw_left_interior;
            side = side.opposite();
        }
        self.add_ring_side(coord, offset_distance, side, left_interior);
    }

    /// Add an offset curve for a ring. The side and left and right topological location arguments are assumed to
    /// be relative to the ring orientation.
    fn add_ring_side(&mut self, coord: &[Coord<f64>], offset_distance: f64, side: Side, left_interior: bool) {
        let curve = self.curve_builder.get_ring_curve(coord, side, offset_distance);
        // If the offset curve has inverted completely it will produce an unwanted artifact in the result, so skip
        // it.
        if let Some(curve) = &curve
            && is_ring_curve_inverted(coord, offset_distance, curve)
        {
            return;
        }
        self.add_curve(curve, left_interior);
    }

    /// Adds the curves for the two sides of a closed linestring (JTS `addLinearRingSides`).
    fn add_linear_ring_sides(&mut self, coord: &[Coord<f64>], distance: f64) {
        // (distance is > 0 due to previous checks)
        let is_hole_computed = !is_ring_fully_eroded(coord, true, distance);
        let is_ccw = is_ccw_area(coord);
        let is_shell_left = !is_ccw;
        if is_shell_left || is_hole_computed {
            let curve = self.curve_builder.get_ring_curve(coord, Side::Left, distance);
            if !matches!(&curve, Some(c) if is_ring_curve_inverted(coord, distance, c)) {
                self.add_curve(curve, false);
            }
        }
        let is_shell_right = is_ccw;
        if is_shell_right || is_hole_computed {
            let curve = self.curve_builder.get_ring_curve(coord, Side::Right, distance);
            if !matches!(&curve, Some(c) if is_ring_curve_inverted(coord, distance, c)) {
                self.add_curve(curve, true);
            }
        }
    }
}

/// JTS `CoordinateArrays.removeRepeatedOrInvalidPoints`.
fn clean(coords: &[Coord<f64>]) -> Vec<Coord<f64>> {
    let mut out: Vec<Coord<f64>> = Vec::with_capacity(coords.len());
    for c in coords {
        if !(c.x.is_finite() && c.y.is_finite()) {
            continue;
        }
        if out.last() != Some(c) {
            out.push(*c);
        }
    }
    out
}

/// Tests whether the offset curve for a ring is fully inverted. An inverted ("inside-out") curve occurs in some
/// specific situations involving a buffer distance which should result in a fully-eroded (empty) buffer. It can
/// happen that the sides of a small, convex polygon produce offset segments which all cross one another to form a
/// curve with inverted orientation. This happens at buffer distances slightly greater than the distance at which
/// the buffer should disappear. The inverted curve will produce an incorrect non-empty buffer (for a shell) or an
/// incorrect hole (for a hole). It must be discarded from the set of offset curves used in the buffer.
///
/// The curve is inverted if all its vertices and segment midpoints are close to the input ring (because the buffer
/// distance is larger than the ring size). This test is only applied to small rings with few vertices.
fn is_ring_curve_inverted(input_ring: &[Coord<f64>], distance: f64, curve_ring: &[Coord<f64>]) -> bool {
    if distance == 0.0 {
        return false;
    }
    // Only proper rings can invert.
    if input_ring.len() <= 3 {
        return false;
    }
    // Heuristic based on low chance that a ring with many vertices will invert. This low limit ensures this test is
    // fairly efficient.
    if input_ring.len() >= MAX_INVERTED_RING_SIZE {
        return false;
    }
    // Don't check curves which are much larger than the input. This improves performance by avoiding checking some
    // concave inputs (which can produce fillet arcs with many more vertices)
    if curve_ring.len() > INVERTED_CURVE_VERTEX_FACTOR * input_ring.len() {
        return false;
    }
    // If curve contains points which are on the buffer, it is not inverted and can be included in the raw curves.
    !has_point_on_buffer(input_ring, distance, curve_ring)
}

/// Tests if there are points on the raw offset curve which may lie on the final buffer curve (i.e. they are
/// (approximately) at the buffer distance from the input ring).
fn has_point_on_buffer(input_ring: &[Coord<f64>], distance: f64, curve_ring: &[Coord<f64>]) -> bool {
    let dist_tol = NEARNESS_FACTOR * distance.abs();
    for i in 0..curve_ring.len() - 1 {
        let v = curve_ring[i];
        // check curve vertices
        if point_to_segment_string(v, input_ring) > dist_tol {
            return true;
        }
        // check curve segment midpoints
        let vnext = curve_ring[i + 1];
        let mid = Coord { x: (v.x + vnext.x) / 2.0, y: (v.y + vnext.y) / 2.0 };
        if point_to_segment_string(mid, input_ring) > dist_tol {
            return true;
        }
    }
    false
}

/// Tests whether a ring buffer is eroded completely (is empty) based on simple heuristics.
///
/// The ring buffer is eroded completely if:
/// - the ring is degenerate (has fewer than 4 vertices)
/// - for a triangle: the buffer distance is larger than the distance of its incentre from a side
/// - for other rings: the buffer is erodable and the envelope is narrower than twice the absolute buffer distance
fn is_ring_fully_eroded(ring: &[Coord<f64>], is_hole: bool, buffer_distance: f64) -> bool {
    // degenerate ring has no area
    if ring.len() < 4 {
        return true;
    }
    // important test to eliminate inverted triangle bug; also optimizes erosion test for triangles
    if ring.len() == 4 {
        return is_triangle_eroded_completely(ring, buffer_distance);
    }
    let is_erodable = (is_hole && buffer_distance > 0.0) || (!is_hole && buffer_distance < 0.0);
    if is_erodable {
        // if envelope is narrower than twice the buffer distance, ring is eroded
        let (mut min_x, mut min_y, mut max_x, mut max_y) =
            (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        for c in ring {
            min_x = min_x.min(c.x);
            min_y = min_y.min(c.y);
            max_x = max_x.max(c.x);
            max_y = max_y.max(c.y);
        }
        let env_min_dimension = (max_y - min_y).min(max_x - min_x);
        if 2.0 * buffer_distance.abs() > env_min_dimension {
            return true;
        }
    }
    false
}

/// Tests whether a triangular ring would be eroded completely by the given buffer distance. This is a precise test.
/// It uses the fact that the inner buffer of a triangle converges on the inCentre of the triangle (the point
/// equidistant from all sides). If the buffer distance is greater than the distance of the inCentre from a side,
/// the triangle will be eroded completely.
///
/// This test is important, since it removes a problematic case where the buffer distance is slightly larger than
/// the inCentre distance. In this case the triangle buffer curve "inverts" with incorrect topology, producing an
/// incorrect hole in the buffer.
fn is_triangle_eroded_completely(tri: &[Coord<f64>], buffer_distance: f64) -> bool {
    let (a, b, c) = (tri[0], tri[1], tri[2]);
    let len_ab = distance(a, b);
    let len_bc = distance(b, c);
    let len_ca = distance(c, a);
    let circum = len_bc + len_ca + len_ab;
    let in_centre = Coord {
        x: (len_bc * a.x + len_ca * b.x + len_ab * c.x) / circum,
        y: (len_bc * a.y + len_ca * b.y + len_ab * c.y) / circum,
    };
    let dist_to_centre = point_to_segment(in_centre, a, b);
    dist_to_centre < buffer_distance.abs()
}

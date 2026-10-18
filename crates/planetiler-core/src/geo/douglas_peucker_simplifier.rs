//! Port of `geo/DouglasPeuckerSimplifier.java`.
//!
//! Simplifies geometries using the Douglas Peucker algorithm without any attempt to repair geometries that become
//! invalid. Adapted from JTS `DouglasPeuckerSimplifier` with modifications to avoid collapsing small polygons (the
//! subsequent area filter removes them more accurately).

use geo_types::{Coord, Geometry};

use crate::geo::geometry_pipeline::GeometryPipeline;
use crate::geo::geometry_transformer::{GeometryTransformer, is_empty};

#[derive(Clone, Copy, Debug)]
pub struct DouglasPeuckerSimplifier {
    sq_tolerance: f64,
}

/// Returns a copy of `geom`, simplified using the Douglas Peucker algorithm.
pub fn simplify(geom: &Geometry<f64>, distance_tolerance: f64) -> Geometry<f64> {
    DouglasPeuckerSimplifier::new(distance_tolerance).simplify(geom)
}

/// Returns a copy of `coords` simplified using the Douglas Peucker algorithm. When `area` is true, retains at least 4
/// points to avoid collapsing a polygon ring.
pub fn simplify_coords(coords: &[Coord<f64>], distance_tolerance: f64, area: bool) -> Vec<Coord<f64>> {
    if coords.is_empty() {
        return Vec::new();
    }
    DouglasPeuckerSimplifier::new(distance_tolerance).simplify_coordinates(coords, area)
}

/// Returns the square of the distance from `(px, py)` to the line segment from `(p1x, p1y)` to `(p2x, p2y)`.
#[inline]
pub fn get_sq_seg_dist(px: f64, py: f64, p1x: f64, p1y: f64, p2x: f64, p2y: f64) -> f64 {
    let mut x = p1x;
    let mut y = p1y;
    let mut dx = p2x - x;
    let mut dy = p2y - y;

    if dx != 0.0 || dy != 0.0 {
        let t = ((px - x) * dx + (py - y) * dy) / (dx * dx + dy * dy);
        if t > 1.0 {
            x = p2x;
            y = p2y;
        } else if t > 0.0 {
            x += dx * t;
            y += dy * t;
        }
    }

    dx = px - x;
    dy = py - y;
    dx * dx + dy * dy
}

impl DouglasPeuckerSimplifier {
    pub fn new(distance_tolerance: f64) -> Self {
        DouglasPeuckerSimplifier { sq_tolerance: distance_tolerance * distance_tolerance.abs() }
    }

    /// Returns a copy of `geom`, simplified using the Douglas Peucker algorithm.
    pub fn simplify(&self, geom: &Geometry<f64>) -> Geometry<f64> {
        if is_empty(geom) || self.sq_tolerance < 0.0 {
            return geom.clone();
        }
        let mut copy = *self;
        copy.transform(geom)
    }

    fn subsimplify(&self, input: &[Coord<f64>], out: &mut Vec<Coord<f64>>, first: usize, last: usize, forced: i32) {
        // forced lets us keep some points even if they are below simplification threshold
        let force = forced > 0;
        let mut max_sq_dist = if force { -1.0 } else { self.sq_tolerance };
        let mut index = 0usize;
        let p1 = input[first];
        let p2 = input[last];

        for (i, c) in input.iter().enumerate().take(last).skip(first + 1) {
            let sq_dist = get_sq_seg_dist(c.x, c.y, p1.x, p1.y, p2.x, p2.y);
            if sq_dist > max_sq_dist {
                index = i;
                max_sq_dist = sq_dist;
            }
        }

        if force || max_sq_dist > self.sq_tolerance {
            if index - first > 1 {
                self.subsimplify(input, out, first, index, forced - 1);
            }
            out.push(input[index]);
            if last - index > 1 {
                self.subsimplify(input, out, index, last, forced - 2);
            }
        }
    }

    /// Simplify a coordinate list (Java's `transformCoordinateList`), always keeping the first and last points.
    pub fn simplify_coordinates(&self, coords: &[Coord<f64>], area: bool) -> Vec<Coord<f64>> {
        let min_points = if area { 4 } else { 2 };
        if coords.len() <= min_points {
            return coords.to_vec();
        }
        let mut result = Vec::with_capacity(coords.len().min(64));
        result.push(coords[0]);
        // for polygons, additionally keep at least 2 intermediate points even if they are below simplification
        // threshold to avoid collapse.
        self.subsimplify(coords, &mut result, 0, coords.len() - 1, min_points as i32 - 2);
        result.push(coords[coords.len() - 1]);
        result
    }
}

impl GeometryTransformer for DouglasPeuckerSimplifier {
    fn transform_coordinates(&mut self, coords: &[Coord<f64>], is_ring: bool) -> Option<Vec<Coord<f64>>> {
        Some(self.simplify_coordinates(coords, is_ring))
    }

    fn remove_degenerate_rings(&self) -> bool {
        true
    }
}

impl GeometryPipeline for DouglasPeuckerSimplifier {
    fn apply(&self, input: &Geometry<f64>) -> Geometry<f64> {
        self.simplify(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::test_utils::*;
    use geo::{CoordsIter, Rotate};
    use geo_types::Point;

    const ROTATIONS: [f64; 5] = [0.0, 45.0, 90.0, 180.0, 270.0];

    fn test_simplify(input: Geometry<f64>, expected: Geometry<f64>, amount: f64) {
        let polygonal = matches!(input, Geometry::Polygon(_) | Geometry::MultiPolygon(_));
        for rotation in ROTATIONS {
            let origin = Point::new(0.0, 0.0);
            let exp_rot = expected.rotate_around_point(rotation, origin);
            let in_rot = input.rotate_around_point(rotation, origin);
            assert_same_normalized_feature(&exp_rot, &simplify(&in_rot, amount));

            // ensure the coordinate list version also works...
            let in_list: Vec<Coord<f64>> = in_rot.coords_iter().collect();
            let exp_list: Vec<Coord<f64>> = exp_rot.coords_iter().collect();
            assert_eq!(exp_list, simplify_coords(&in_list, amount, polygonal), "rotation={rotation}");
        }
    }

    #[test]
    fn dont_modify_point() {
        test_simplify(new_point(1.0, 1.0).into(), new_point(1.0, 1.0).into(), 1.0);
    }

    #[test]
    fn simplify_2_points() {
        test_simplify(
            new_line_string(&[0.0, 0.0, 10.0, 10.0]).into(),
            new_line_string(&[0.0, 0.0, 10.0, 10.0]).into(),
            1.0,
        );
    }

    #[test]
    fn remove_a_point() {
        test_simplify(
            new_line_string(&[0.0, 0.0, 5.0, 0.9, 10.0, 0.0]).into(),
            new_line_string(&[0.0, 0.0, 10.0, 0.0]).into(),
            1.0,
        );
    }

    #[test]
    fn keep_a_point() {
        test_simplify(
            new_line_string(&[0.0, 0.0, 5.0, 1.1, 10.0, 0.0]).into(),
            new_line_string(&[0.0, 0.0, 5.0, 1.1, 10.0, 0.0]).into(),
            1.0,
        );
    }

    #[test]
    fn polygon_leave_a_point() {
        test_simplify(
            rectangle(0.0, 10.0).into(),
            new_polygon(&[0.0, 0.0, 10.0, 0.0, 10.0, 10.0, 0.0, 0.0]).into(),
            20.0,
        );
    }
}

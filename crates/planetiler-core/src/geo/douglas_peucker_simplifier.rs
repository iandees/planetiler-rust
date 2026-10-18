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

/// Same as [`simplify_coords`] but appends the result to `out`.
pub fn simplify_coords_into(coords: &[Coord<f64>], distance_tolerance: f64, area: bool, out: &mut Vec<Coord<f64>>) {
    if !coords.is_empty() {
        DouglasPeuckerSimplifier::new(distance_tolerance).simplify_coordinates_into(coords, area, out);
    }
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

/// Returns the index of the first point strictly between `first` and `last` with the greatest
/// [`get_sq_seg_dist`] from the segment `input[first]`-`input[last]` that is greater than `init`, and that distance
/// (or `(0, init)` if there is none).
#[inline]
fn furthest_point(input: &[Coord<f64>], first: usize, last: usize, init: f64) -> (usize, f64) {
    let p1 = input[first];
    let p2 = input[last];
    let pts = &input[first + 1..last];
    // same arithmetic as get_sq_seg_dist with the per-segment terms hoisted out of the loop
    let dx = p2.x - p1.x;
    let dy = p2.y - p1.y;
    let (index, max) = if dx != 0.0 || dy != 0.0 {
        let den = dx * dx + dy * dy;
        argmax(pts, init, |c| {
            let t = ((c.x - p1.x) * dx + (c.y - p1.y) * dy) / den;
            let (x, y) = if t > 1.0 {
                (p2.x, p2.y)
            } else if t > 0.0 {
                (p1.x + dx * t, p1.y + dy * t)
            } else {
                (p1.x, p1.y)
            };
            let (ex, ey) = (c.x - x, c.y - y);
            ex * ex + ey * ey
        })
    } else {
        argmax(pts, init, |c| {
            let (ex, ey) = (c.x - p1.x, c.y - p1.y);
            ex * ex + ey * ey
        })
    };
    match index {
        Some(i) => (first + 1 + i, max),
        None => (0, init),
    }
}

/// Index of the first element with the greatest `f(element)` that is strictly greater than `init`. Uses 4
/// independent lanes so the comparisons don't form one long dependency chain; the result is the same as a
/// sequential scan.
#[inline(always)]
fn argmax(pts: &[Coord<f64>], init: f64, f: impl Fn(&Coord<f64>) -> f64) -> (Option<usize>, f64) {
    const LANES: usize = 4;
    let mut best = [init; LANES];
    let mut idx = [usize::MAX; LANES];
    let (chunks, rem) = pts.as_chunks::<LANES>();
    for (ci, chunk) in chunks.iter().enumerate() {
        for l in 0..LANES {
            let d = f(&chunk[l]);
            if d > best[l] {
                best[l] = d;
                idx[l] = ci * LANES + l;
            }
        }
    }
    let base = pts.len() - rem.len();
    for (l, c) in rem.iter().enumerate() {
        let d = f(c);
        if d > best[l] {
            best[l] = d;
            idx[l] = base + l;
        }
    }
    let mut result = (None, init);
    for l in 0..LANES {
        if idx[l] != usize::MAX {
            let better = match result.0 {
                None => true,
                Some(i) => best[l] > result.1 || (best[l] == result.1 && idx[l] < i),
            };
            if better {
                result = (Some(idx[l]), best[l]);
            }
        }
    }
    result
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
        let (index, max_sq_dist) = furthest_point(input, first, last, if force { -1.0 } else { self.sq_tolerance });

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
        self.simplify_coordinates_into(coords, area, &mut result);
        result
    }

    /// Same as [`DouglasPeuckerSimplifier::simplify_coordinates`] but appends the result to `out`.
    pub fn simplify_coordinates_into(&self, coords: &[Coord<f64>], area: bool, out: &mut Vec<Coord<f64>>) {
        let min_points = if area { 4 } else { 2 };
        if coords.len() <= min_points {
            out.extend_from_slice(coords);
            return;
        }
        out.push(coords[0]);
        // for polygons, additionally keep at least 2 intermediate points even if they are below simplification
        // threshold to avoid collapse.
        self.subsimplify(coords, out, 0, coords.len() - 1, min_points as i32 - 2);
        out.push(coords[coords.len() - 1]);
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

    #[test]
    fn furthest_point_matches_sequential_scan() {
        // pseudo-random points from a small set of values so there are exact ties and repeated endpoints
        let mut seed = 12345u64;
        let mut next = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((seed >> 33) % 20) as f64 / 4.0
        };
        for n in 2..40 {
            let coords: Vec<Coord<f64>> = (0..n).map(|_| Coord { x: next(), y: next() }).collect();
            for first in 0..n - 1 {
                for last in first + 1..n {
                    for init in [-1.0, 0.0, 0.5, 2.0] {
                        let (p1, p2) = (coords[first], coords[last]);
                        let mut max = init;
                        let mut index = 0;
                        for (i, c) in coords.iter().enumerate().take(last).skip(first + 1) {
                            let d = get_sq_seg_dist(c.x, c.y, p1.x, p1.y, p2.x, p2.y);
                            if d > max {
                                index = i;
                                max = d;
                            }
                        }
                        let (i, m) = furthest_point(&coords, first, last, init);
                        assert_eq!((index, max.to_bits()), (i, m.to_bits()), "n={n} first={first} last={last}");
                    }
                }
            }
        }
    }

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

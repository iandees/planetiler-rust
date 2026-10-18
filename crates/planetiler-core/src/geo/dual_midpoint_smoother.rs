//! Port of `geo/DualMidpointSmoother.java`: smooths an input geometry by interpolating 2 points at certain ratios
//! along each edge and repeating for a set number of iterations. When the points are `[0.25, 0.75]` this is
//! equivalent to [Chaikin Smoothing](https://observablehq.com/@pamacha/chaikins-algorithm).

use geo_types::{Coord, Geometry};

use crate::geo::douglas_peucker_simplifier::get_sq_seg_dist;
use crate::geo::geometry_pipeline::GeometryPipeline;
use crate::geo::geometry_transformer::GeometryTransformer;
use crate::geo::mutable_coordinate_sequence::MutableCoordinateSequence;
use crate::geo::vw_simplifier::triangle_area;

#[derive(Clone, Copy, Debug)]
pub struct DualMidpointSmoother {
    a: f64,
    b: f64,
    iters: usize,
    min_vertex_area: f64,
    min_squared_vertex_offset: f64,
    max_vertex_area: f64,
    max_squared_vertex_offset: f64,
}

impl DualMidpointSmoother {
    pub fn new(a: f64, b: f64) -> Self {
        DualMidpointSmoother {
            a,
            b,
            iters: 1,
            min_vertex_area: 0.0,
            min_squared_vertex_offset: 0.0,
            max_vertex_area: 0.0,
            max_squared_vertex_offset: 0.0,
        }
    }

    /// Chaikin smoothing `iters` times.
    pub fn chaikin(iters: usize) -> Self {
        Self::new(0.25, 0.75).set_iters(iters)
    }

    /// Chaikin smoothing that stops when added points would get dropped by Douglas-Peucker simplification with
    /// `tolerance`.
    pub fn chaikin_to_tolerance(tolerance: f64) -> Self {
        Self::chaikin(10).set_min_vertex_offset(tolerance)
    }

    /// Chaikin smoothing that stops when added points would get dropped by Visvalingam-Whyatt simplification with
    /// `min_area`.
    pub fn chaikin_to_min_area(min_area: f64) -> Self {
        Self::chaikin(10).set_min_vertex_area(min_area)
    }

    /// Sets the min distance between a vertex and the line between its neighbors below which a vertex will not be
    /// squashed.
    pub fn set_min_vertex_offset(mut self, min_vertex_offset: f64) -> Self {
        self.min_squared_vertex_offset = min_vertex_offset * min_vertex_offset.abs();
        self
    }

    /// Sets the min area of the triangle created by a point and its 2 neighbors below which a vertex will not be
    /// squashed.
    pub fn set_min_vertex_area(mut self, min_vertex_area: f64) -> Self {
        self.min_vertex_area = min_vertex_area;
        self
    }

    /// Sets a limit on the maximum area that can be removed when removing a vertex.
    pub fn set_max_vertex_area(mut self, max_area: f64) -> Self {
        self.max_vertex_area = max_area;
        self
    }

    /// Sets a limit on the maximum distance from the original shape that can be introduced by smoothing.
    pub fn set_max_vertex_offset(mut self, max_vertex_offset: f64) -> Self {
        self.max_squared_vertex_offset = max_vertex_offset * max_vertex_offset.abs();
        self
    }

    /// Sets the maximum number of times that smoothing runs.
    pub fn set_iters(mut self, iters: usize) -> Self {
        self.iters = iters;
        self
    }

    pub fn smooth_coordinates(&self, input: &[Coord<f64>], area: bool) -> Vec<Coord<f64>> {
        if input.len() <= 2 {
            return input.to_vec();
        }
        let mut coords = input.to_vec();
        for _ in 0..self.iters {
            let mut result = MutableCoordinateSequence::with_capacity(coords.len() * 2);
            let last = coords.len() - 1;
            let (mut x2, mut y2) = (coords[0].x, coords[0].y);
            let (mut x3, mut y3) = (coords[1].x, coords[1].y);
            // for lines, always add the first point but for polygons this is a placeholder that will be updated
            // when last vertex is squashed
            result.add_point(x2, y2);
            for i in 1..last {
                let (x1, y1) = (x2, y2);
                (x2, y2) = (x3, y3);
                (x3, y3) = (coords[i + 1].x, coords[i + 1].y);
                self.squash_vertex(&mut result, x1, y1, x2, y2, x3, y3);
            }
            if area {
                self.squash_vertex(
                    &mut result,
                    coords[last - 1].x,
                    coords[last - 1].y,
                    coords[0].x,
                    coords[0].y,
                    coords[1].x,
                    coords[1].y,
                );
                let idx = result.size() - 1;
                let (lx, ly) = (result.get_x(idx), result.get_y(idx));
                result.set_x(0, lx);
                result.set_y(0, ly);
            } else {
                result.add_point(coords[last].x, coords[last].y);
            }
            // early exit case: if no vertices were collapsed then we are done smoothing
            if coords.len() == result.size() {
                return result.into_coords();
            }
            coords = result.into_coords();
        }
        coords
    }

    #[allow(clippy::too_many_arguments)]
    fn squash_vertex(
        &self,
        result: &mut MutableCoordinateSequence,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        x3: f64,
        y3: f64,
    ) {
        if self.skip_vertex(x1, y1, x2, y2, x3, y3) {
            result.add_point(x2, y2);
            return;
        }

        let mut next_b = self.b;
        let mut next_a = self.a;

        // check the amount of error introduced by removing this vertex (either by offset or area)
        // and if it is too large, then move nextA/nextB ratios closer to the vertex to limit the error
        if self.max_vertex_area > 0.0 || self.max_squared_vertex_offset > 0.0 {
            let mag_a = (x2 - x1).hypot(y2 - y1);
            let mag_b = (x3 - x2).hypot(y3 - y2);
            let den = mag_a * mag_b;
            let a_dist = mag_a * (1.0 - self.b);
            let b_dist = mag_b * self.a;
            let mut max_dist_squared = f64::INFINITY;
            if self.max_vertex_area > 0.0 {
                let sin =
                    if den <= 0.0 { 0.0 } else { (((x1 - x2) * (y3 - y2)) - ((y1 - y2) * (x3 - x2))).abs() / den };
                if sin != 0.0 {
                    max_dist_squared = 2.0 * self.max_vertex_area / sin;
                }
            }
            if self.max_squared_vertex_offset > 0.0 {
                let cos = if den <= 0.0 {
                    0.0
                } else {
                    (((x1 - x2) * (x3 - x2) + (y1 - y2) * (y3 - y2)) / den).clamp(-1.0, 1.0)
                };
                max_dist_squared = max_dist_squared.min(2.0 * self.max_squared_vertex_offset / (1.0 + cos));
            }
            let mut max_dist = f64::NAN;
            if a_dist * a_dist > max_dist_squared {
                max_dist = max_dist_squared.sqrt();
                next_b = 1.0 - max_dist / mag_a;
            }
            if b_dist * b_dist > max_dist_squared {
                if max_dist.is_nan() {
                    max_dist = max_dist_squared.sqrt();
                }
                next_a = max_dist / mag_b;
            }
        }

        result.add_point(x1 + (x2 - x1) * next_b, y1 + (y2 - y1) * next_b);
        result.add_point(x2 + (x3 - x2) * next_a, y2 + (y3 - y2) * next_a);
    }

    fn skip_vertex(&self, x1: f64, y1: f64, x2: f64, y2: f64, x3: f64, y3: f64) -> bool {
        (self.min_vertex_area > 0.0 && triangle_area(x1, y1, x2, y2, x3, y3) < self.min_vertex_area)
            || (self.min_squared_vertex_offset > 0.0
                && get_sq_seg_dist(x2, y2, x1, y1, x3, y3) < self.min_squared_vertex_offset)
    }
}

impl GeometryTransformer for DualMidpointSmoother {
    fn transform_coordinates(&mut self, coords: &[Coord<f64>], is_ring: bool) -> Option<Vec<Coord<f64>>> {
        Some(self.smooth_coordinates(coords, is_ring))
    }
}

impl GeometryPipeline for DualMidpointSmoother {
    fn apply(&self, input: &Geometry<f64>) -> Geometry<f64> {
        let mut copy = *self;
        copy.transform(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::midpoint_smoother::tests::wkt;
    use crate::geo::test_utils::round;

    fn check(smoother: DualMidpointSmoother, cases: &[(&str, &str)]) {
        for (input, expected) in cases {
            assert_eq!(round(&wkt(expected)), round(&smoother.apply(&wkt(input))), "{input}");
        }
    }

    #[test]
    fn dual_midpoint_smooth() {
        check(
            DualMidpointSmoother::new(0.2, 0.8).set_iters(1),
            &[
                ("POINT(1 1)", "POINT(1 1)"),
                ("LINESTRING(0 0, 10 10)", "LINESTRING(0 0, 10 10)"),
                ("LINESTRING(0 0, 10 0, 10 10)", "LINESTRING(0 0, 8 0, 10 2, 10 10)"),
                ("LINESTRING(0 0, 10 0, 10 10, 0 10)", "LINESTRING(0 0, 8 0, 10 2, 10 8, 8 10, 0 10)"),
                ("POLYGON((0 0, 10 0, 10 10, 0 0))", "POLYGON((2 0, 8 0, 10 2, 10 8, 8 8, 2 2, 2 0))"),
                (
                    "POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))",
                    "POLYGON((2 0, 8 0, 10 2, 10 8, 8 10, 2 10, 0 8, 0 2, 2 0))",
                ),
            ],
        );
    }

    #[test]
    fn multi_pass_smooth() {
        check(
            DualMidpointSmoother::new(0.2, 0.8).set_iters(2),
            &[
                ("POINT(1 1)", "POINT(1 1)"),
                ("LINESTRING(0 0, 10 10)", "LINESTRING(0 0, 10 10)"),
                ("LINESTRING(0 0, 10 0, 10 10)", "LINESTRING(0 0, 6.4 0, 8.4 0.4, 9.6 1.6, 10 3.6, 10 10)"),
                (
                    "POLYGON((0 0, 10 0, 10 10, 0 0))",
                    "POLYGON((3.2 0, 6.8 0, 8.4 0.4, 9.6 1.6, 10 3.2, 10 6.8, 9.6 8, 8.4 8, 6.8 6.8, 3.2 3.2, 2 1.6, 2 0.4, 3.2 0))",
                ),
            ],
        );
    }

    #[test]
    fn smooth_to_tolerance() {
        check(
            DualMidpointSmoother::new(0.2, 0.8).set_iters(200).set_min_vertex_offset(0.5),
            &[
                ("POINT(1 1)", "POINT(1 1)"),
                ("LINESTRING(0 0, 10 10)", "LINESTRING(0 0, 10 10)"),
                (
                    "LINESTRING(0 0, 10 0, 10 10)",
                    "LINESTRING (0 0, 6.4 0, 8 0.32, 8.64 0.64, 9.36 1.36, 9.68 2, 10 3.6, 10 10)",
                ),
                (
                    "POLYGON((0 0, 10 0, 10 10, 0 0))",
                    "POLYGON ((3.2 0, 6.8 0, 8.4 0.4, 9.6 1.6, 10 3.2, 10 6.8, 9.68 7.76, 9.36 8, 8.4 8, 6.8 6.8, 3.2 3.2, 2 1.6, 2 0.64, 2.24 0.32, 3.2 0))",
                ),
            ],
        );
    }

    #[test]
    fn smooth_to_min_area() {
        check(
            DualMidpointSmoother::new(0.2, 0.8).set_iters(200).set_min_vertex_area(0.5),
            &[
                ("POINT(1 1)", "POINT(1 1)"),
                ("LINESTRING(0 0, 10 10)", "LINESTRING(0 0, 10 10)"),
                (
                    "LINESTRING(0 0, 10 0, 10 10)",
                    "LINESTRING (0 0, 5.12 0, 6.8 0.08, 8 0.32, 8.64 0.64, 9.36 1.36, 9.68 2, 9.92 3.2, 10 4.88, 10 10)",
                ),
                (
                    "POLYGON((0 0, 10 0, 10 10, 0 0))",
                    "POLYGON ((3.92 0, 6.08 0, 7.12 0.08, 8.08 0.32, 8.64 0.64, 9.36 1.36, 9.68 1.92, 9.92 2.88, 10 3.92, 10 6.08, 9.92 7.04, 9.68 7.76, 9.36 8, 8.64 8, 8.08 7.76, 7.12 7.04, 6.08 6.08, 3.92 3.92, 2.96 2.88, 2.24 1.92, 2 1.36, 2 0.64, 2.24 0.32, 2.96 0.08, 3.92 0))",
                ),
            ],
        );
    }

    #[test]
    fn smooth_with_max_area() {
        check(
            DualMidpointSmoother::chaikin(1).set_max_vertex_area(0.5),
            &[
                ("LINESTRING(0 0, 10 10)", "LINESTRING(0 0, 10 10)"),
                ("LINESTRING(0 0, 10 0, 10 10)", "LINESTRING (0 0, 9 0, 10 1, 10 10)"),
                ("LINESTRING(0 0, 10 0, 20 0)", "LINESTRING (0 0, 7.5 0, 12.5 0, 20 0)"),
                ("LINESTRING(0 0, 10 0, 0 0)", "LINESTRING (0 0, 7.5 0, 0 0)"),
                (
                    "POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))",
                    "POLYGON ((1 0, 9 0, 10 1, 10 9, 9 10, 1 10, 0 9, 0 1, 1 0))",
                ),
            ],
        );
    }

    #[test]
    fn smooth_with_max_offset() {
        check(
            DualMidpointSmoother::chaikin(1).set_max_vertex_offset(0.5f64.sqrt()),
            &[
                ("LINESTRING(0 0, 10 10)", "LINESTRING(0 0, 10 10)"),
                ("LINESTRING(0 0, 10 0, 10 10)", "LINESTRING (0 0, 9 0, 10 1, 10 10)"),
                ("LINESTRING(0 0, 10 0, 20 0)", "LINESTRING (0 0, 7.5 0, 12.5 0, 20 0)"),
                ("LINESTRING(0 0, 10 0, 10 5)", "LINESTRING (0 0, 9 0, 10 1, 10 5)"),
                (
                    "POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))",
                    "POLYGON ((1 0, 9 0, 10 1, 10 9, 9 10, 1 10, 0 9, 0 1, 1 0))",
                ),
            ],
        );
    }
}

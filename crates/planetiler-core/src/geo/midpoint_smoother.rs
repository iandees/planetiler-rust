//! Port of `geo/MidpointSmoother.java`: smooths an input geometry by iteratively joining the midpoint of each edge of
//! lines or polygons.
//!
//! See <https://github.com/wipfli/midpoint-smoothing>.

use geo_types::{Coord, Geometry};

use crate::geo::geometry_pipeline::GeometryPipeline;
use crate::geo::geometry_transformer::GeometryTransformer;
use crate::geo::mutable_coordinate_sequence::MutableCoordinateSequence;

#[derive(Clone, Copy, Debug)]
pub struct MidpointSmoother {
    ratio: f64,
    iters: usize,
}

impl Default for MidpointSmoother {
    fn default() -> Self {
        Self::new()
    }
}

impl MidpointSmoother {
    pub fn new() -> Self {
        Self::with_ratio(0.5)
    }

    pub fn with_ratio(ratio: f64) -> Self {
        MidpointSmoother { ratio, iters: 1 }
    }

    /// Sets the number of times that smoothing runs.
    pub fn set_iters(mut self, iters: usize) -> Self {
        self.iters = iters;
        self
    }

    pub fn smooth_coordinates(&self, coords: &[Coord<f64>], area: bool) -> Vec<Coord<f64>> {
        if coords.len() <= 2 {
            return coords.to_vec();
        }
        let mut coords = coords.to_vec();
        for _ in 0..self.iters {
            let mut result = MutableCoordinateSequence::with_capacity(coords.len() + 1);
            if !area {
                result.add_point(coords[0].x, coords[0].y);
            }
            let last = coords.len() - 1;
            for i in 0..last {
                let (a, b) = (coords[i], coords[i + 1]);
                result.add_point(a.x + (b.x - a.x) * self.ratio, a.y + (b.y - a.y) * self.ratio);
            }
            if area {
                result.close_ring();
            } else {
                result.add_point(coords[last].x, coords[last].y);
            }
            coords = result.into_coords();
        }
        coords
    }
}

impl GeometryTransformer for MidpointSmoother {
    fn transform_coordinates(&mut self, coords: &[Coord<f64>], is_ring: bool) -> Option<Vec<Coord<f64>>> {
        Some(self.smooth_coordinates(coords, is_ring))
    }
}

impl GeometryPipeline for MidpointSmoother {
    fn apply(&self, input: &Geometry<f64>) -> Geometry<f64> {
        let mut copy = *self;
        copy.transform(input)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::geo::test_utils::round;

    /// Parse a WKT test fixture.
    pub(crate) fn wkt(input: &str) -> Geometry<f64> {
        crate::geo::wkt::read_wkt(input).expect("invalid wkt")
    }

    fn check(input: &str, expected: &str, iters: usize) {
        let result = MidpointSmoother::new().set_iters(iters).apply(&wkt(input));
        assert_eq!(round(&wkt(expected)), round(&result), "{input}");
    }

    #[test]
    fn midpoint_smooth() {
        for (input, output) in [
            ("POINT(1 1)", "POINT(1 1)"),
            ("LINESTRING(0 0, 10 10)", "LINESTRING(0 0, 10 10)"),
            ("LINESTRING(0 0, 10 0, 10 10)", "LINESTRING(0 0, 5 0, 10 5, 10 10)"),
            ("LINESTRING(0 0, 10 0, 10 10, 0 10)", "LINESTRING(0 0, 5 0, 10 5, 5 10, 0 10)"),
            ("POLYGON((0 0, 10 0, 10 10, 0 0))", "POLYGON((5 0, 10 5, 5 5, 5 0))"),
            ("POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))", "POLYGON((5 0, 10 5, 5 10, 0 5, 5 0))"),
        ] {
            check(input, output, 1);
        }
    }

    #[test]
    fn midpoint_smooth_twice() {
        for (input, output) in [
            ("POINT(1 1)", "POINT(1 1)"),
            ("LINESTRING(0 0, 10 10)", "LINESTRING(0 0, 10 10)"),
            ("LINESTRING(0 0, 10 0, 10 10)", "LINESTRING(0 0, 2.5 0, 7.5 2.5, 10 7.5, 10 10)"),
            ("POLYGON((0 0, 10 0, 10 10, 0 0))", "POLYGON ((7.5 2.5, 7.5 5, 5 2.5, 7.5 2.5))"),
            ("POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))", "POLYGON ((7.5 2.5, 7.5 7.5, 2.5 7.5, 2.5 2.5, 7.5 2.5))"),
        ] {
            check(input, output, 2);
        }
    }
}

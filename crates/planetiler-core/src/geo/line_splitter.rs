//! Port of `geo/LineSplitter.java`: utility for extracting sub-ranges of a line.

use std::sync::OnceLock;

use geo_types::{Coord, Geometry, LineString, Point};

use crate::geo::geometry_exception::{GeometryException, GeometryResult};
use crate::geo::mutable_coordinate_sequence::MutableCoordinateSequence;

/// Extracts points or partial lines at ratios along a line string (`0` = start, `1` = end).
#[derive(Debug)]
pub struct LineSplitter {
    line: LineString<f64>,
    /// `(total length, cumulative length at each node)`
    node_locations: OnceLock<(f64, Vec<f64>)>,
}

impl LineSplitter {
    pub fn new(line: LineString<f64>) -> Self {
        LineSplitter { line, node_locations: OnceLock::new() }
    }

    /// Fails unless `geom` is a line string.
    pub fn from_geometry(geom: &Geometry<f64>) -> GeometryResult<Self> {
        match geom {
            Geometry::LineString(ls) => Ok(Self::new(ls.clone())),
            Geometry::Line(l) => Ok(Self::new((*l).into())),
            other => Err(GeometryException::new(
                "line_splitter_not_line",
                format!("Expected LineString, got {:?}", crate::geo::geometry_type::GeometryType::type_of(other)),
            )),
        }
    }

    pub fn line(&self) -> &LineString<f64> {
        &self.line
    }

    fn init(&self) -> (f64, &[f64]) {
        let (len, locs) = self.node_locations.get_or_init(|| {
            let cs = &self.line.0;
            let mut locations = Vec::with_capacity(cs.len());
            let mut length = 0.0;
            locations.push(0.0);
            for w in cs.windows(2) {
                let dx = w[1].x - w[0].x;
                let dy = w[1].y - w[0].y;
                length += (dx * dx + dy * dy).sqrt();
                locations.push(length);
            }
            (length, locations)
        });
        (*len, locs)
    }

    /// Returns a point at `ratio` along this line where 0 is the beginning of the line and 1 is the end.
    ///
    /// Panics if `ratio` is not between 0 and 1.
    pub fn get_point(&self, ratio: f64) -> Point<f64> {
        assert!((0.0..=1.0).contains(&ratio), "Invalid ratio: {ratio}");
        let (length, locs) = self.init();
        let pos = ratio * length;
        let idx = lower_index(locs, pos).max(0) as usize;
        let mut result = MutableCoordinateSequence::with_capacity(1);
        add_interpolated(&mut result, &self.line.0, locs, idx, pos);
        Point(result.get(0))
    }

    /// Returns a partial segment of this line from `start` to `end` where 0 is the beginning of the line and 1 is the
    /// end.
    ///
    /// Panics if the range is invalid.
    pub fn get(&self, start: f64, end: f64) -> LineString<f64> {
        assert!(!(start < 0.0 || end > 1.0 || end < start), "Invalid range: {start} to {end}");
        if start <= 0.0 && end >= 1.0 {
            return self.line.clone();
        }
        let cs = &self.line.0;
        let (length, locs) = self.init();
        let mut result = MutableCoordinateSequence::new();
        let start_pos = start * length;
        let end_pos = end * length;
        let first = floor_index(locs, start_pos);
        let last = lower_index(locs, end_pos);
        add_interpolated(&mut result, cs, locs, first as usize, start_pos);
        for c in cs.iter().take((last + 1).max(0) as usize).skip((first + 1) as usize) {
            result.add_point(c.x, c.y);
        }
        add_interpolated(&mut result, cs, locs, last.max(0) as usize, end_pos);
        result.into()
    }
}

/// Java `Arrays.binarySearch` semantics: `Ok(index)` or `Err(insertion point)`.
fn binary_search(locs: &[f64], value: f64) -> Result<usize, usize> {
    locs.binary_search_by(|probe| probe.partial_cmp(&value).unwrap_or(std::cmp::Ordering::Less))
}

fn floor_index(locs: &[f64], length: f64) -> isize {
    match binary_search(locs, length) {
        Ok(idx) => idx as isize,
        Err(ins) => ins as isize - 1,
    }
}

fn lower_index(locs: &[f64], length: f64) -> isize {
    match binary_search(locs, length) {
        Ok(idx) => idx as isize - 1,
        Err(ins) => ins as isize - 1,
    }
}

fn add_interpolated(
    result: &mut MutableCoordinateSequence,
    cs: &[Coord<f64>],
    locs: &[f64],
    start_idx: usize,
    position: f64,
) {
    let end_idx = start_idx + 1;
    let start_pos = locs[start_idx];
    let end_pos = locs[end_idx];
    let (p1, p2) = (cs[start_idx], cs[end_idx]);
    let ratio = (position - start_pos) / (end_pos - start_pos);
    result.add_point(p1.x + (p2.x - p1.x) * ratio, p1.y + (p2.y - p1.y) * ratio);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::test_utils::*;

    #[test]
    fn single_segment() {
        for (start, end) in [(0.0, 1.0), (0.0, 0.25), (0.75, 1.0), (0.25, 0.75)] {
            let l = LineSplitter::new(new_line_string(&[0.0, 0.0, 2.0, 1.0]));
            assert_eq!(new_line_string(&[start * 2.0, start, end * 2.0, end]), l.get(start, end));
        }
    }

    #[test]
    fn length2() {
        let l = LineSplitter::new(new_line_string(&[0.0, 0.0, 1.0, 2.0, 2.0, 4.0]));
        assert_eq!(new_line_string(&[0.0, 0.0, 0.5, 1.0]), l.get(0.0, 0.25));
        assert_eq!(new_line_string(&[0.2, 0.4, 0.5, 1.0]), l.get(0.1, 0.25));
        assert_eq!(new_line_string(&[0.5, 1.0, 1.0, 2.0]), l.get(0.25, 0.5));
        assert_eq!(new_line_string(&[0.5, 1.0, 1.0, 2.0, 1.5, 3.0]), l.get(0.25, 0.75));
        assert_eq!(new_line_string(&[1.0, 2.0, 1.5, 3.0]), l.get(0.5, 0.75));
        assert_eq!(new_line_string(&[1.2, 2.4, 1.5, 3.0]), l.get(0.6, 0.75));
        assert_eq!(new_line_string(&[1.5, 3.0, 2.0, 4.0]), l.get(0.75, 1.0));
    }

    #[test]
    fn distance_along_line() {
        for ratio in [0.0, 0.00001, 0.1, 0.49999, 0.5, 0.50001, 0.9, 0.99999, 1.0] {
            let l = LineSplitter::new(new_line_string(&[0.0, 0.0, 1.0, 0.5, 2.0, 1.0]));
            assert_eq!(
                round(&new_point(ratio * 2.0, ratio).into()),
                round(&l.get_point(ratio).into()),
                "ratio={ratio}"
            );
        }
    }

    #[test]
    fn invalid() {
        let l = LineSplitter::new(new_line_string(&[0.0, 0.0, 1.0, 2.0, 2.0, 4.0]));
        for (start, end) in [(-0.1, 0.5), (0.9, 1.1), (0.6, 0.5)] {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| l.get(start, end)));
            assert!(result.is_err(), "{start} to {end}");
        }
    }

    #[test]
    fn from_geometry_rejects_non_lines() {
        assert!(LineSplitter::from_geometry(&new_point(0.0, 0.0).into()).is_err());
        assert!(LineSplitter::from_geometry(&new_line_string(&[0.0, 0.0, 1.0, 1.0]).into()).is_ok());
    }
}

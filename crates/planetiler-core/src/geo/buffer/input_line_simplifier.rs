//! Port of JTS `BufferInputLineSimplifier`.
//!
//! Simplifies a buffer input line to remove concavities with shallow depth. The most important benefit of doing this
//! is to reduce the number of points and the complexity of shape which will be buffered. It also reduces the risk of
//! gores created by the quantized fillet arcs (in round-joined buffers).
//!
//! The simplification only removes vertices on the side of the line being buffered (concavities on the left for a
//! positive tolerance, on the right for a negative one), so the buffer result is not changed by more than the
//! tolerance.

use geo_types::Coord;

use crate::geo::cg_algorithms::{CLOCKWISE, COUNTERCLOCKWISE, is_ring, orientation_index, point_to_segment};

const NUM_PTS_TO_CHECK: usize = 10;

/// Simplifies `input_line`: if `distance_tol` is positive, concavities on the left side of the line are simplified;
/// if it is negative, concavities on the right side.
pub fn simplify(input_line: &[Coord<f64>], distance_tol: f64) -> Vec<Coord<f64>> {
    let mut simp = Simplifier {
        input: input_line,
        distance_tol: distance_tol.abs(),
        is_ring: is_ring(input_line),
        is_deleted: vec![false; input_line.len()],
        angle_orientation: if distance_tol < 0.0 { CLOCKWISE } else { COUNTERCLOCKWISE },
    };
    while simp.delete_shallow_concavities() {}
    simp.collapse_line()
}

struct Simplifier<'a> {
    input: &'a [Coord<f64>],
    distance_tol: f64,
    is_ring: bool,
    is_deleted: Vec<bool>,
    angle_orientation: i32,
}

impl Simplifier<'_> {
    /// Uses a sliding window containing 3 vertices to detect shallow angles in which the middle vertex can be
    /// deleted, since it does not affect the shape of the resulting buffer in a significant way.
    fn delete_shallow_concavities(&mut self) -> bool {
        // do not simplify end line segments of lines (this ensures that end caps are generated consistently)
        let mut index = if self.is_ring { 0 } else { 1 };
        let mut mid_index = self.next_index(index);
        let mut last_index = self.next_index(mid_index);
        let mut is_changed = false;
        while last_index < self.input.len() {
            let mut is_middle_vertex_deleted = false;
            if self.is_deletable(index, mid_index, last_index) {
                self.is_deleted[mid_index] = true;
                is_middle_vertex_deleted = true;
                is_changed = true;
            }
            index = if is_middle_vertex_deleted { last_index } else { mid_index };
            mid_index = self.next_index(index);
            last_index = self.next_index(mid_index);
        }
        is_changed
    }

    /// The next non-deleted index, or the length of the input if there are none.
    fn next_index(&self, index: usize) -> usize {
        let mut next = index + 1;
        while next < self.input.len() && self.is_deleted[next] {
            next += 1;
        }
        next
    }

    fn collapse_line(&self) -> Vec<Coord<f64>> {
        // JTS uses `ArrayList.add`, which keeps repeated points
        self.input.iter().zip(&self.is_deleted).filter(|(_, deleted)| !**deleted).map(|(c, _)| *c).collect()
    }

    fn is_deletable(&self, i0: usize, i1: usize, i2: usize) -> bool {
        let p0 = self.input[i0];
        let p1 = self.input[i1];
        let p2 = self.input[i2];
        if !self.is_concave(p0, p1, p2) {
            return false;
        }
        if !is_shallow(p0, p1, p2, self.distance_tol) {
            return false;
        }
        // NOTE: JTS passes p1 (not p2) as the end coordinate of the section here
        self.is_shallow_sampled(p0, p1, i0, i2)
    }

    /// Checks for shallowness over a sample of points in the given section. This helps prevents the simplification
    /// from incrementally "skipping" over points which are in fact non-shallow.
    fn is_shallow_sampled(&self, p0: Coord<f64>, p2: Coord<f64>, i0: usize, i2: usize) -> bool {
        let mut inc = (i2 - i0) / NUM_PTS_TO_CHECK;
        if inc == 0 {
            inc = 1;
        }
        let mut i = i0;
        while i < i2 {
            if !is_shallow(p0, self.input[i], p2, self.distance_tol) {
                return false;
            }
            i += inc;
        }
        true
    }

    fn is_concave(&self, p0: Coord<f64>, p1: Coord<f64>, p2: Coord<f64>) -> bool {
        orientation_index(p0, p1, p2) == self.angle_orientation
    }
}

fn is_shallow(p0: Coord<f64>, p1: Coord<f64>, p2: Coord<f64>, distance_tol: f64) -> bool {
    point_to_segment(p1, p0, p2) < distance_tol
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::test_utils::new_coordinate_list;

    #[test]
    fn removes_shallow_concavity_on_buffered_side() {
        // a tiny dent away from the left side of an eastward line (a concavity on the left side)
        let line = new_coordinate_list(&[0.0, 0.0, 5.0, 0.0, 6.0, -0.001, 7.0, 0.0, 10.0, 0.0]);
        let simplified = simplify(&line, 0.01);
        assert_eq!(new_coordinate_list(&[0.0, 0.0, 5.0, 0.0, 7.0, 0.0, 10.0, 0.0]), simplified);
        // on the right side the dent itself is convex and kept, but the shallow right turn after it is removed
        assert_eq!(new_coordinate_list(&[0.0, 0.0, 5.0, 0.0, 6.0, -0.001, 10.0, 0.0]), simplify(&line, -0.01));
        // a deep dent is kept
        let deep = new_coordinate_list(&[0.0, 0.0, 5.0, 0.0, 6.0, -1.0, 7.0, 0.0, 10.0, 0.0]);
        assert_eq!(deep, simplify(&deep, 0.01));
    }
}

//! Port of JTS `OffsetCurveBuilder` (the raw offset curves used by the buffer operation).
//!
//! Computes the raw offset curve for a single geometry component (ring, line or point). A raw offset curve line is
//! not noded - it may contain self-intersections (and usually will). The final buffer polygon is computed by forming
//! a topological graph of all the noded raw curves and tracing outside contours. The points in the raw curve are
//! rounded to a given precision model.

use geo_types::Coord;

use super::input_line_simplifier;
use super::offset_segment_generator::{OffsetSegmentGenerator, Side};
use super::parameters::{BufferParameters, CapStyle};
use crate::geo::precision_model::PrecisionModel;

pub struct OffsetCurveBuilder {
    params: BufferParameters,
    precision: PrecisionModel,
}

impl OffsetCurveBuilder {
    /// A builder for offset curves whose points are rounded to `precision`.
    pub fn new(params: BufferParameters, precision: PrecisionModel) -> OffsetCurveBuilder {
        OffsetCurveBuilder { params, precision }
    }

    pub fn buffer_parameters(&self) -> &BufferParameters {
        &self.params
    }

    /// The offset curve for a line or point, as a closed ring (`None` for an empty curve).
    ///
    /// For a positive distance (or negative for single-sided buffers) this is the buffer outline around the line.
    pub fn get_line_curve(&self, input_pts: &[Coord<f64>], distance: f64) -> Option<Vec<Coord<f64>>> {
        if self.is_line_offset_empty(distance) {
            return None;
        }
        let pos_distance = distance.abs();
        let mut seg_gen = self.seg_gen(pos_distance);
        if input_pts.len() <= 1 {
            self.compute_point_curve(input_pts[0], &mut seg_gen);
        } else if self.params.is_single_sided {
            let is_right_side = distance < 0.0;
            self.compute_single_sided_buffer_curve(input_pts, is_right_side, pos_distance, &mut seg_gen);
        } else {
            self.compute_line_buffer_curve(input_pts, pos_distance, &mut seg_gen);
        }
        Some(seg_gen.into_coordinates())
    }

    /// Whether the offset curve generated for a line or point is empty: a zero width buffer of a line or point is
    /// empty, as is a negative width buffer (except for single-sided buffers, where the sign indicates the side).
    pub fn is_line_offset_empty(&self, distance: f64) -> bool {
        distance == 0.0 || (distance < 0.0 && !self.params.is_single_sided)
    }

    /// The offset curve of a ring on one side (`side`) at `distance`, which must be non-negative.
    ///
    /// The curve follows the direction of the input ring. Returns `None` for an empty curve.
    pub fn get_ring_curve(&self, input_pts: &[Coord<f64>], side: Side, distance: f64) -> Option<Vec<Coord<f64>>> {
        if input_pts.len() <= 2 {
            return self.get_line_curve(input_pts, distance);
        }
        // optimize creating ring for for zero distance
        if distance == 0.0 {
            return Some(input_pts.to_vec());
        }
        let mut seg_gen = self.seg_gen(distance);
        self.compute_ring_buffer_curve(input_pts, side, distance, &mut seg_gen);
        Some(seg_gen.into_coordinates())
    }

    fn seg_gen(&self, distance: f64) -> OffsetSegmentGenerator {
        OffsetSegmentGenerator::new(&self.params, distance, self.precision)
    }

    /// The distance tolerance to use during input line simplification.
    fn simplify_tolerance(&self, buf_distance: f64) -> f64 {
        buf_distance * self.params.simplify_factor()
    }

    fn compute_point_curve(&self, pt: Coord<f64>, seg_gen: &mut OffsetSegmentGenerator) {
        match self.params.end_cap_style {
            CapStyle::Round => seg_gen.create_circle(pt),
            CapStyle::Square => seg_gen.create_square(pt),
            // otherwise curve is empty (e.g. for a butt cap)
            CapStyle::Flat => {}
        }
    }

    fn compute_line_buffer_curve(&self, input_pts: &[Coord<f64>], distance: f64, seg_gen: &mut OffsetSegmentGenerator) {
        let dist_tol = self.simplify_tolerance(distance);

        // compute points for left side of line
        // Simplify the appropriate side of the line before generating
        let simp1 = input_line_simplifier::simplify(input_pts, dist_tol);
        let n1 = simp1.len() - 1;
        seg_gen.init_side_segments(simp1[0], simp1[1], Side::Left);
        for p in &simp1[2..=n1] {
            seg_gen.add_next_segment(*p, true);
        }
        seg_gen.add_last_segment();
        // add line cap for end of line
        seg_gen.add_line_end_cap(simp1[n1 - 1], simp1[n1]);

        // compute points for right side of line
        let simp2 = input_line_simplifier::simplify(input_pts, -dist_tol);
        let n2 = simp2.len() - 1;
        // since we are traversing line in opposite order, offset position is still LEFT
        seg_gen.init_side_segments(simp2[n2], simp2[n2 - 1], Side::Left);
        for i in (0..n2.saturating_sub(1)).rev() {
            seg_gen.add_next_segment(simp2[i], true);
        }
        seg_gen.add_last_segment();
        // add line cap for start of line
        seg_gen.add_line_end_cap(simp2[1], simp2[0]);

        seg_gen.close_ring();
    }

    fn compute_single_sided_buffer_curve(
        &self,
        input_pts: &[Coord<f64>],
        is_right_side: bool,
        distance: f64,
        seg_gen: &mut OffsetSegmentGenerator,
    ) {
        let dist_tol = self.simplify_tolerance(distance);
        if is_right_side {
            // add original line
            seg_gen.add_segments(input_pts, true);
            // compute points for right side of line
            let simp2 = input_line_simplifier::simplify(input_pts, -dist_tol);
            let n2 = simp2.len() - 1;
            // since we are traversing line in opposite order, offset position is still LEFT
            seg_gen.init_side_segments(simp2[n2], simp2[n2 - 1], Side::Left);
            seg_gen.add_first_segment();
            for i in (0..n2.saturating_sub(1)).rev() {
                seg_gen.add_next_segment(simp2[i], true);
            }
        } else {
            // add original line
            seg_gen.add_segments(input_pts, false);
            // compute points for left side of line
            let simp1 = input_line_simplifier::simplify(input_pts, dist_tol);
            let n1 = simp1.len() - 1;
            seg_gen.init_side_segments(simp1[0], simp1[1], Side::Left);
            seg_gen.add_first_segment();
            for p in &simp1[2..=n1] {
                seg_gen.add_next_segment(*p, true);
            }
        }
        seg_gen.add_last_segment();
        seg_gen.close_ring();
    }

    fn compute_ring_buffer_curve(
        &self,
        input_pts: &[Coord<f64>],
        side: Side,
        distance: f64,
        seg_gen: &mut OffsetSegmentGenerator,
    ) {
        // simplify input line to improve performance
        let mut dist_tol = self.simplify_tolerance(distance);
        // ensure that correct side is simplified
        if side == Side::Right {
            dist_tol = -dist_tol;
        }
        let simp = input_line_simplifier::simplify(input_pts, dist_tol);
        let n = simp.len() - 1;
        seg_gen.init_side_segments(simp[n - 1], simp[0], side);
        for (i, p) in simp.iter().enumerate().skip(1) {
            let add_start_point = i != 1;
            seg_gen.add_next_segment(*p, add_start_point);
        }
        seg_gen.close_ring();
    }
}

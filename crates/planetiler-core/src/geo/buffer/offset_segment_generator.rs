//! Port of JTS `OffsetSegmentGenerator` and `OffsetSegmentString`.
//!
//! Generates segments which form an offset curve. Supports all end cap and join options provided for buffering. The
//! output coordinates are rounded to a precision model (floating except for JTS's fixed precision fallback).

use geo_types::Coord;

use super::parameters::{BufferParameters, CapStyle, JoinStyle};
use crate::geo::cg_algorithms::angle::{self, PI_OVER_2, PI_TIMES_2};
use crate::geo::cg_algorithms::{
    CLOCKWISE, COUNTERCLOCKWISE, LineIntersector, atan2, distance as dist, intersection, line_segment_intersection,
    orientation_index, point_to_segment,
};
use crate::geo::precision_model::PrecisionModel;

/// JTS `Position`: which side of a line to offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

impl Side {
    pub fn opposite(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
        }
    }
}

/// Factor which controls how close offset segments can be to skip adding a fillet or mitre.
const OFFSET_SEGMENT_SEPARATION_FACTOR: f64 = 0.05;
/// Factor which controls how close curve vertices on inside turns can be to be snapped.
const INSIDE_TURN_VERTEX_SNAP_DISTANCE_FACTOR: f64 = 1.0E-3;
/// Factor which controls how close curve vertices can be to be snapped.
const CURVE_VERTEX_SNAP_DISTANCE_FACTOR: f64 = 1.0E-4;
/// Factor which determines how short closing segs can be for round buffers.
const MAX_CLOSING_SEG_LEN_FACTOR: i32 = 80;

/// JTS `OffsetSegmentString`: a dynamic list of the vertices in a constructed offset curve, which skips vertices
/// within a minimum distance of the previous one.
#[derive(Debug)]
struct OffsetSegmentString {
    pts: Vec<Coord<f64>>,
    precision: PrecisionModel,
    minimum_vertex_distance: f64,
}

impl OffsetSegmentString {
    #[inline]
    fn add_pt(&mut self, pt: Coord<f64>) {
        let pt = self.precision.make_precise_coord(pt);
        // don't add duplicate (or near-duplicate) points
        if let Some(last) = self.pts.last()
            && dist(pt, *last) < self.minimum_vertex_distance
        {
            return;
        }
        self.pts.push(pt);
    }

    fn add_pts(&mut self, pts: &[Coord<f64>], is_forward: bool) {
        if is_forward {
            for p in pts {
                self.add_pt(*p);
            }
        } else {
            for p in pts.iter().rev() {
                self.add_pt(*p);
            }
        }
    }

    fn close_ring(&mut self) {
        if let (Some(first), Some(last)) = (self.pts.first().copied(), self.pts.last())
            && first != *last
        {
            self.pts.push(first);
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Segment {
    p0: Coord<f64>,
    p1: Coord<f64>,
}

/// JTS `OffsetSegmentGenerator`.
pub struct OffsetSegmentGenerator {
    /// The angle quantum with which to approximate a fillet curve (based on the input # of quadrant segments).
    fillet_angle_quantum: f64,
    /// The closing segment length factor used when generating "inside turn" closing segments.
    closing_seg_length_factor: i32,
    seg_list: OffsetSegmentString,
    distance: f64,
    params: BufferParameters,
    li: LineIntersector,
    s0: Coord<f64>,
    s1: Coord<f64>,
    s2: Coord<f64>,
    seg0: Segment,
    seg1: Segment,
    offset0: Segment,
    offset1: Segment,
    side: Side,
    has_narrow_concave_angle: bool,
}

impl OffsetSegmentGenerator {
    /// A generator for offset curves at `distance` whose points are rounded to `precision`.
    pub fn new(params: &BufferParameters, distance: f64, precision: PrecisionModel) -> OffsetSegmentGenerator {
        let quad_segs = params.quadrant_segments.max(1);
        let closing_seg_length_factor = if params.quadrant_segments >= 8 && params.join_style == JoinStyle::Round {
            MAX_CLOSING_SEG_LEN_FACTOR
        } else {
            1
        };
        let distance = distance.abs();
        OffsetSegmentGenerator {
            fillet_angle_quantum: PI_OVER_2 / quad_segs as f64,
            closing_seg_length_factor,
            seg_list: OffsetSegmentString {
                pts: Vec::new(),
                precision,
                minimum_vertex_distance: distance * CURVE_VERTEX_SNAP_DISTANCE_FACTOR,
            },
            distance,
            params: *params,
            li: LineIntersector::new(),
            s0: Coord::zero(),
            s1: Coord::zero(),
            s2: Coord::zero(),
            seg0: Segment::default(),
            seg1: Segment::default(),
            offset0: Segment::default(),
            offset1: Segment::default(),
            side: Side::Left,
            has_narrow_concave_angle: false,
        }
    }

    /// Whether the input has a narrow concave angle (relative to the offset distance), in which case the generated
    /// offset curve will contain self-intersections and heuristic closing segments.
    pub fn has_narrow_concave_angle(&self) -> bool {
        self.has_narrow_concave_angle
    }

    pub fn init_side_segments(&mut self, s1: Coord<f64>, s2: Coord<f64>, side: Side) {
        self.s1 = s1;
        self.s2 = s2;
        self.side = side;
        self.seg1 = Segment { p0: s1, p1: s2 };
        self.offset1 = compute_offset_segment(self.seg1, side, self.distance);
    }

    pub fn into_coordinates(self) -> Vec<Coord<f64>> {
        self.seg_list.pts
    }

    pub fn close_ring(&mut self) {
        self.seg_list.close_ring();
    }

    pub fn add_segments(&mut self, pts: &[Coord<f64>], is_forward: bool) {
        self.seg_list.add_pts(pts, is_forward);
    }

    pub fn add_first_segment(&mut self) {
        self.seg_list.add_pt(self.offset1.p0);
    }

    /// Add last offset point.
    pub fn add_last_segment(&mut self) {
        self.seg_list.add_pt(self.offset1.p1);
    }

    pub fn add_next_segment(&mut self, p: Coord<f64>, add_start_point: bool) {
        // s0-s1-s2 are the coordinates of the previous segment and the current one
        self.s0 = self.s1;
        self.s1 = self.s2;
        self.s2 = p;
        self.seg0 = Segment { p0: self.s0, p1: self.s1 };
        self.offset0 = compute_offset_segment(self.seg0, self.side, self.distance);
        self.seg1 = Segment { p0: self.s1, p1: self.s2 };
        self.offset1 = compute_offset_segment(self.seg1, self.side, self.distance);

        // do nothing if points are equal
        if self.s1 == self.s2 {
            return;
        }
        let orientation = orientation_index(self.s0, self.s1, self.s2);
        let outside_turn = (orientation == CLOCKWISE && self.side == Side::Left)
            || (orientation == COUNTERCLOCKWISE && self.side == Side::Right);
        if orientation == 0 {
            // lines are collinear
            self.add_collinear(add_start_point);
        } else if outside_turn {
            self.add_outside_turn(orientation, add_start_point);
        } else {
            self.add_inside_turn();
        }
    }

    fn add_collinear(&mut self, add_start_point: bool) {
        // This test could probably be done more efficiently, but the situation of exact collinearity should be fairly
        // rare.
        self.li.compute_intersection(self.s0, self.s1, self.s1, self.s2);
        let num_int = self.li.intersection_num();
        // if numInt is < 2, the lines are parallel and in the same direction. In this case the point can be ignored,
        // since the offset lines will also be parallel.
        if num_int >= 2 {
            // segments are collinear but reversing. Add an "end-cap" fillet all the way around to other direction.
            // This case should ONLY happen for LineStrings, so the orientation is always CW. (Polygons can never
            // have two consecutive segments which are parallel but reversed, because that would be a self
            // intersection.)
            if matches!(self.params.join_style, JoinStyle::Bevel | JoinStyle::Mitre) {
                if add_start_point {
                    self.seg_list.add_pt(self.offset0.p1);
                }
                self.seg_list.add_pt(self.offset1.p0);
            } else {
                self.add_corner_fillet(self.s1, self.offset0.p1, self.offset1.p0, CLOCKWISE, self.distance);
            }
        }
    }

    /// Adds the offset points for an outside (convex) turn.
    fn add_outside_turn(&mut self, orientation: i32, add_start_point: bool) {
        // Heuristic: If offset endpoints are very close together, (which happens for nearly-parallel segments), use
        // an endpoint as the single offset corner vertex. This eliminates very short single-segment joins, which
        // reduces the number of offset curve vertices. This also avoids robustness problems with computing mitre
        // corners for nearly-parallel segments.
        if dist(self.offset0.p1, self.offset1.p0) < self.distance * OFFSET_SEGMENT_SEPARATION_FACTOR {
            // use endpoint of longest segment, to reduce change in area
            let seg_len0 = dist(self.s0, self.s1);
            let seg_len1 = dist(self.s1, self.s2);
            let offset_pt = if seg_len0 > seg_len1 { self.offset0.p1 } else { self.offset1.p0 };
            self.seg_list.add_pt(offset_pt);
            return;
        }
        match self.params.join_style {
            JoinStyle::Mitre => self.add_mitre_join(self.s1, self.offset0, self.offset1, self.distance),
            JoinStyle::Bevel => self.add_bevel_join(self.offset0, self.offset1),
            JoinStyle::Round => {
                // add a circular fillet connecting the endpoints of the offset segments
                if add_start_point {
                    self.seg_list.add_pt(self.offset0.p1);
                }
                self.add_corner_fillet(self.s1, self.offset0.p1, self.offset1.p0, orientation, self.distance);
                self.seg_list.add_pt(self.offset1.p0);
            }
        }
    }

    /// Adds the offset points for an inside (concave) turn.
    fn add_inside_turn(&mut self) {
        // add intersection point of offset segments (if any)
        self.li.compute_intersection(self.offset0.p0, self.offset0.p1, self.offset1.p0, self.offset1.p1);
        if self.li.has_intersection() {
            self.seg_list.add_pt(self.li.intersection(0));
        } else {
            // If no intersection is detected, it means the angle is so small and/or the offset so large that the
            // offsets segments don't intersect. In this case we must add a "closing segment" to make sure the buffer
            // curve is continuous, fairly smooth (e.g. no sharp reversals in direction) and tracks the buffer
            // correctly around the corner. The curve connects the endpoints of the segment offsets to points which
            // lie toward the centre point of the corner. The joining curve will not appear in the final buffer
            // outline, since it is completely internal to the buffer polygon.
            //
            // The intersection test above is vulnerable to robustness errors; i.e. it may be that the offsets should
            // intersect very close to their endpoints, but aren't reported as such due to rounding. To handle this
            // situation appropriately, we use the following test: If the offset points are very close, don't add
            // closing segments but simply use one of the offset points
            self.has_narrow_concave_angle = true;
            if dist(self.offset0.p1, self.offset1.p0) < self.distance * INSIDE_TURN_VERTEX_SNAP_DISTANCE_FACTOR {
                self.seg_list.add_pt(self.offset0.p1);
            } else {
                // add endpoint of this segment offset
                self.seg_list.add_pt(self.offset0.p1);
                // add "closing segment" of required length
                if self.closing_seg_length_factor > 0 {
                    let f = self.closing_seg_length_factor as f64;
                    let s1 = self.s1;
                    let (o0, o1) = (self.offset0.p1, self.offset1.p0);
                    let mid0 = Coord { x: (f * o0.x + s1.x) / (f + 1.0), y: (f * o0.y + s1.y) / (f + 1.0) };
                    self.seg_list.add_pt(mid0);
                    let mid1 = Coord { x: (f * o1.x + s1.x) / (f + 1.0), y: (f * o1.y + s1.y) / (f + 1.0) };
                    self.seg_list.add_pt(mid1);
                } else {
                    // This branch is not expected to be used except for testing purposes. It is equivalent to the
                    // JTS 1.9 logic for closing segments (which results in very poor performance for large buffer
                    // distances)
                    self.seg_list.add_pt(self.s1);
                }
                // add start point of next segment offset
                self.seg_list.add_pt(self.offset1.p0);
            }
        }
    }

    /// Adds an end cap around point `p1`, terminating a line segment coming from `p0`.
    pub fn add_line_end_cap(&mut self, p0: Coord<f64>, p1: Coord<f64>) {
        let seg = Segment { p0, p1 };
        let offset_l = compute_offset_segment(seg, Side::Left, self.distance);
        let offset_r = compute_offset_segment(seg, Side::Right, self.distance);
        let dx = p1.x - p0.x;
        let dy = p1.y - p0.y;
        let angle = atan2(dy, dx);
        match self.params.end_cap_style {
            CapStyle::Round => {
                // add offset seg points with a fillet between them
                self.seg_list.add_pt(offset_l.p1);
                self.add_directed_fillet(p1, angle + PI_OVER_2, angle - PI_OVER_2, CLOCKWISE, self.distance);
                self.seg_list.add_pt(offset_r.p1);
            }
            CapStyle::Flat => {
                // only offset segment points are added
                self.seg_list.add_pt(offset_l.p1);
                self.seg_list.add_pt(offset_r.p1);
            }
            CapStyle::Square => {
                // add a square defined by extensions of the offset segment endpoints
                let side_x = self.distance.abs() * angle::cos_snap(angle);
                let side_y = self.distance.abs() * angle::sin_snap(angle);
                self.seg_list.add_pt(Coord { x: offset_l.p1.x + side_x, y: offset_l.p1.y + side_y });
                self.seg_list.add_pt(Coord { x: offset_r.p1.x + side_x, y: offset_r.p1.y + side_y });
            }
        }
    }

    /// Adds a mitre join connecting two convex offset segments. The mitre is beveled if it exceeds the mitre limit
    /// factor. The mitre limit is intended to prevent extremely long corners occurring. If the mitre limit is very
    /// small it can cause unwanted artifacts around fairly flat corners. This is prevented by using a simple bevel
    /// join in this case. In other words, the limit prevents the corner from getting too long, but it won't force it
    /// to be very short/flat.
    fn add_mitre_join(&mut self, corner_pt: Coord<f64>, offset0: Segment, offset1: Segment, distance: f64) {
        let mitre_limit_distance = self.params.mitre_limit * distance;
        // First try a non-beveled join. Compute the intersection point of the lines determined by the offsets.
        // Parallel or collinear lines will return a null point ==> need to be beveled
        //
        // Note: This computation is unstable if the offset segments are nearly collinear. However, this situation
        // should have been eliminated earlier by the check for whether the offset segment endpoints are almost
        // coincident
        if let Some(int_pt) = intersection(offset0.p0, offset0.p1, offset1.p0, offset1.p1)
            && dist(int_pt, corner_pt) <= mitre_limit_distance
        {
            self.seg_list.add_pt(int_pt);
            return;
        }
        // In case the mitre limit is very small, try a plain bevel. Use it if it's further than the limit.
        let bevel_dist = point_to_segment(corner_pt, offset0.p1, offset1.p0);
        if bevel_dist >= mitre_limit_distance {
            self.add_bevel_join(offset0, offset1);
            return;
        }
        // Have to construct a limited mitre bevel.
        self.add_limited_mitre_join(offset0, offset1, distance, mitre_limit_distance);
    }

    /// Adds a limited mitre join connecting two convex offset segments. A limited mitre join is beveled at the
    /// distance determined by the mitre limit factor, or as a standard bevel join, whichever is further.
    fn add_limited_mitre_join(&mut self, offset0: Segment, offset1: Segment, distance: f64, mitre_limit_distance: f64) {
        let corner_pt = self.seg0.p1;
        // oriented angle of the corner formed by segments
        let ang_interior = angle::angle_between_oriented(self.seg0.p0, corner_pt, self.seg1.p1);
        // half of the interior angle
        let ang_interior2 = ang_interior / 2.0;
        // direction of bisector of the interior angle between the segments
        let dir0 = angle::angle(corner_pt, self.seg0.p0);
        let dir_bisector = angle::normalize(dir0 + ang_interior2);
        // midpoint of the bevel segment
        let bevel_mid_pt = project(corner_pt, -mitre_limit_distance, dir_bisector);
        // direction of bevel segment (at right angle to corner bisector)
        let dir_bevel = angle::normalize(dir_bisector + PI_OVER_2);
        // compute the candidate bevel segment by projecting both sides of the midpoint
        let bevel0 = project(bevel_mid_pt, distance, dir_bevel);
        let bevel1 = project(bevel_mid_pt, distance, dir_bevel + std::f64::consts::PI);
        // compute actual bevel segment between the offset lines
        let bevel_int0 = line_segment_intersection(offset0.p0, offset0.p1, bevel0, bevel1);
        let bevel_int1 = line_segment_intersection(offset1.p0, offset1.p1, bevel0, bevel1);
        // add the limited bevel, if it intersects the offsets
        if let (Some(b0), Some(b1)) = (bevel_int0, bevel_int1) {
            self.seg_list.add_pt(b0);
            self.seg_list.add_pt(b1);
            return;
        }
        // If the corner is very flat or the mitre limit is very small the limited bevel segment may not intersect
        // the offsets. In this case just bevel the join.
        self.add_bevel_join(offset0, offset1);
    }

    /// Adds a bevel join connecting the two offset segments around a reflex corner.
    fn add_bevel_join(&mut self, offset0: Segment, offset1: Segment) {
        self.seg_list.add_pt(offset0.p1);
        self.seg_list.add_pt(offset1.p0);
    }

    /// Add points for a circular fillet around a reflex corner. Adds the start and end points.
    fn add_corner_fillet(&mut self, p: Coord<f64>, p0: Coord<f64>, p1: Coord<f64>, direction: i32, radius: f64) {
        let mut start_angle = atan2(p0.y - p.y, p0.x - p.x);
        let end_angle = atan2(p1.y - p.y, p1.x - p.x);
        if direction == CLOCKWISE {
            if start_angle <= end_angle {
                start_angle += PI_TIMES_2;
            }
        } else if start_angle >= end_angle {
            start_angle -= PI_TIMES_2;
        }
        self.seg_list.add_pt(p0);
        self.add_directed_fillet(p, start_angle, end_angle, direction, radius);
        self.seg_list.add_pt(p1);
    }

    /// Adds points for a circular fillet arc between two specified angles. The start and end point for the fillet
    /// are not added - the caller must add them if required.
    fn add_directed_fillet(&mut self, p: Coord<f64>, start_angle: f64, end_angle: f64, direction: i32, radius: f64) {
        let direction_factor = if direction == CLOCKWISE { -1.0 } else { 1.0 };
        let total_angle = (start_angle - end_angle).abs();
        let n_segs = (total_angle / self.fillet_angle_quantum + 0.5) as i32;
        if n_segs < 1 {
            // no segments because angle is less than increment - nothing to do!
            return;
        }
        // choose angle increment so that each segment has equal length
        let angle_inc = total_angle / n_segs as f64;
        for i in 0..n_segs {
            let angle = start_angle + direction_factor * i as f64 * angle_inc;
            self.seg_list
                .add_pt(Coord { x: p.x + radius * angle::cos_snap(angle), y: p.y + radius * angle::sin_snap(angle) });
        }
    }

    /// Creates a CW circle around a point.
    pub fn create_circle(&mut self, p: Coord<f64>) {
        // add start point
        self.seg_list.add_pt(Coord { x: p.x + self.distance, y: p.y });
        self.add_directed_fillet(p, 0.0, PI_TIMES_2, -1, self.distance);
        self.seg_list.close_ring();
    }

    /// Creates a CW square around a point.
    pub fn create_square(&mut self, p: Coord<f64>) {
        let d = self.distance;
        self.seg_list.add_pt(Coord { x: p.x + d, y: p.y + d });
        self.seg_list.add_pt(Coord { x: p.x + d, y: p.y - d });
        self.seg_list.add_pt(Coord { x: p.x - d, y: p.y - d });
        self.seg_list.add_pt(Coord { x: p.x - d, y: p.y + d });
        self.seg_list.close_ring();
    }
}

/// Projects a point to a given distance in a given direction angle.
fn project(pt: Coord<f64>, d: f64, dir: f64) -> Coord<f64> {
    Coord { x: pt.x + d * angle::cos_snap(dir), y: pt.y + d * angle::sin_snap(dir) }
}

/// Compute an offset segment for an input segment on a given side and at a given distance. The offset points are
/// computed in full double precision, for accuracy.
#[inline]
fn compute_offset_segment(seg: Segment, side: Side, distance: f64) -> Segment {
    let side_sign = if side == Side::Left { 1.0 } else { -1.0 };
    let dx = seg.p1.x - seg.p0.x;
    let dy = seg.p1.y - seg.p0.y;
    let len = (dx * dx + dy * dy).sqrt();
    // u is the vector that is the length of the offset, in the direction of the segment
    let ux = side_sign * distance * dx / len;
    let uy = side_sign * distance * dy / len;
    Segment { p0: Coord { x: seg.p0.x - uy, y: seg.p0.y + ux }, p1: Coord { x: seg.p1.x - uy, y: seg.p1.y + ux } }
}

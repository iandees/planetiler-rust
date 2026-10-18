//! Ports of the small JTS computational geometry primitives used by the buffer and polygonize ports, with the same
//! floating point behaviour as JTS (so results are bit-for-bit identical):
//!
//! - `math.DD` (double-double arithmetic, only the operations JTS uses below) -> [`DD`]
//! - `CGAlgorithmsDD.orientationIndex` / `Orientation.index` -> [`orientation_index`]
//! - `CGAlgorithmsDD.intersection` / `Intersection.intersection` -> [`intersection`]
//! - `Intersection.lineSegment` -> [`line_segment_intersection`]
//! - `Distance.pointToSegment`, `pointToSegmentString`, `pointToLinePerpendicular`
//! - `RobustLineIntersector` (segment/segment, 2D only) -> [`LineIntersector`]
//! - `Angle` helpers

use geo_types::Coord;

pub const COUNTERCLOCKWISE: i32 = 1;
pub const CLOCKWISE: i32 = -1;
pub const COLLINEAR: i32 = 0;

/// JTS `DD` double-double number (only the operations used by the ported algorithms).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DD {
    hi: f64,
    lo: f64,
}

const SPLIT: f64 = 134217729.0; // 2^27+1, for IEEE double

impl DD {
    #[inline]
    pub fn new(x: f64) -> DD {
        DD { hi: x, lo: 0.0 }
    }

    #[inline]
    fn is_nan(&self) -> bool {
        self.hi.is_nan()
    }

    /// `selfAdd(double y)`
    #[inline]
    pub fn add_f64(self, y: f64) -> DD {
        let s_big = self.hi + y;
        let e = s_big - self.hi;
        let mut s = s_big - e;
        s = (y - e) + (self.hi - s);
        let f = s + self.lo;
        let h_big = s_big + f;
        let h = f + (s_big - h_big);
        let hi = h_big + h;
        let lo = h + (h_big - hi);
        DD { hi, lo }
    }

    /// `selfAdd(double yhi, double ylo)`
    #[inline]
    fn add_dd(self, yhi: f64, ylo: f64) -> DD {
        let s_big = self.hi + yhi;
        let t_big = self.lo + ylo;
        let mut e = s_big - self.hi;
        let f = t_big - self.lo;
        let mut s = s_big - e;
        let mut t = t_big - f;
        s = (yhi - e) + (self.hi - s);
        t = (ylo - f) + (self.lo - t);
        e = s + t_big;
        let h_big = s_big + e;
        let h = e + (s_big - h_big);
        e = t + h;
        let zhi = h_big + e;
        let zlo = e + (h_big - zhi);
        DD { hi: zhi, lo: zlo }
    }

    /// `selfSubtract(double y)`
    #[inline]
    pub fn subtract_f64(self, y: f64) -> DD {
        if self.is_nan() {
            return self;
        }
        self.add_dd(-y, 0.0)
    }

    /// `selfSubtract(DD y)`
    #[inline]
    pub fn subtract(self, y: DD) -> DD {
        if self.is_nan() {
            return self;
        }
        self.add_dd(-y.hi, -y.lo)
    }

    /// `selfMultiply(double yhi, double ylo)`
    #[inline]
    fn mul_dd(self, yhi: f64, ylo: f64) -> DD {
        let hi = self.hi;
        let mut c_big = SPLIT * hi;
        let mut hx = c_big - hi;
        let mut c = SPLIT * yhi;
        hx = c_big - hx;
        let tx = hi - hx;
        let mut hy = c - yhi;
        c_big = hi * yhi;
        hy = c - hy;
        let ty = yhi - hy;
        c = ((((hx * hy - c_big) + hx * ty) + tx * hy) + tx * ty) + (hi * ylo + self.lo * yhi);
        let zhi = c_big + c;
        hx = c_big - zhi;
        let zlo = c + hx;
        DD { hi: zhi, lo: zlo }
    }

    /// `selfMultiply(double y)`
    #[inline]
    pub fn multiply_f64(self, y: f64) -> DD {
        self.mul_dd(y, 0.0)
    }

    /// `multiply(DD y)` / `selfMultiply(DD y)`
    #[inline]
    pub fn multiply(self, y: DD) -> DD {
        if y.is_nan() {
            return DD { hi: f64::NAN, lo: f64::NAN };
        }
        self.mul_dd(y.hi, y.lo)
    }

    /// `selfDivide(DD y)`
    #[inline]
    pub fn divide(self, y: DD) -> DD {
        let (yhi, ylo) = (y.hi, y.lo);
        let c_big = self.hi / yhi;
        let mut c = SPLIT * c_big;
        let mut hc = c - c_big;
        let mut u = SPLIT * yhi;
        hc = c - hc;
        let tc = c_big - hc;
        let mut hy = u - yhi;
        let u_big = c_big * yhi;
        hy = u - hy;
        let ty = yhi - hy;
        u = (((hc * hy - u_big) + hc * ty) + tc * hy) + tc * ty;
        c = ((((self.hi - u_big) - u) + self.lo) - c_big * ylo) / yhi;
        u = c_big + c;
        DD { hi: u, lo: (c_big - u) + c }
    }

    #[inline]
    pub fn signum(&self) -> i32 {
        if self.hi > 0.0 {
            1
        } else if self.hi < 0.0 {
            -1
        } else if self.lo > 0.0 {
            1
        } else if self.lo < 0.0 {
            -1
        } else {
            0
        }
    }

    #[inline]
    pub fn double_value(&self) -> f64 {
        self.hi + self.lo
    }
}

#[inline]
fn signum(x: f64) -> i32 {
    if x > 0.0 {
        1
    } else if x < 0.0 {
        -1
    } else {
        0
    }
}

const DP_SAFE_EPSILON: f64 = 1e-15;

#[inline]
fn orientation_index_filter(pax: f64, pay: f64, pbx: f64, pby: f64, pcx: f64, pcy: f64) -> i32 {
    let detleft = (pax - pcx) * (pby - pcy);
    let detright = (pay - pcy) * (pbx - pcx);
    let det = detleft - detright;
    let detsum = if detleft > 0.0 {
        if detright <= 0.0 {
            return signum(det);
        }
        detleft + detright
    } else if detleft < 0.0 {
        if detright >= 0.0 {
            return signum(det);
        }
        -detleft - detright
    } else {
        return signum(det);
    };
    let errbound = DP_SAFE_EPSILON * detsum;
    if det >= errbound || -det >= errbound {
        return signum(det);
    }
    2
}

/// JTS `Orientation.index(p1, p2, q)`: 1 if `q` is left of (counter-clockwise from) `p1->p2`, -1 if right, 0 if
/// collinear.
#[inline]
pub fn orientation_index(p1: Coord<f64>, p2: Coord<f64>, q: Coord<f64>) -> i32 {
    let index = orientation_index_filter(p1.x, p1.y, p2.x, p2.y, q.x, q.y);
    if index <= 1 {
        return index;
    }
    let dx1 = DD::new(p2.x).add_f64(-p1.x);
    let dy1 = DD::new(p2.y).add_f64(-p1.y);
    let dx2 = DD::new(q.x).add_f64(-p2.x);
    let dy2 = DD::new(q.y).add_f64(-p2.y);
    dx1.multiply(dy2).subtract(dy1.multiply(dx2)).signum()
}

/// JTS `CGAlgorithmsDD.intersection`: intersection of the lines through `p1-p2` and `q1-q2`, or `None` if they are
/// parallel.
pub fn intersection(p1: Coord<f64>, p2: Coord<f64>, q1: Coord<f64>, q2: Coord<f64>) -> Option<Coord<f64>> {
    let px = DD::new(p1.y).subtract_f64(p2.y);
    let py = DD::new(p2.x).subtract_f64(p1.x);
    let pw = DD::new(p1.x).multiply_f64(p2.y).subtract(DD::new(p2.x).multiply_f64(p1.y));

    let qx = DD::new(q1.y).subtract_f64(q2.y);
    let qy = DD::new(q2.x).subtract_f64(q1.x);
    let qw = DD::new(q1.x).multiply_f64(q2.y).subtract(DD::new(q2.x).multiply_f64(q1.y));

    let x = py.multiply(qw).subtract(qy.multiply(pw));
    let y = qx.multiply(pw).subtract(px.multiply(qw));
    let w = px.multiply(qy).subtract(qx.multiply(py));

    let x_int = x.divide(w).double_value();
    let y_int = y.divide(w).double_value();
    if !x_int.is_finite() || !y_int.is_finite() {
        return None;
    }
    Some(Coord { x: x_int, y: y_int })
}

/// JTS `Intersection.lineSegment`: intersection of the infinite line `line1-line2` with the segment `seg1-seg2`.
pub fn line_segment_intersection(
    line1: Coord<f64>,
    line2: Coord<f64>,
    seg1: Coord<f64>,
    seg2: Coord<f64>,
) -> Option<Coord<f64>> {
    let orient_s1 = orientation_index(line1, line2, seg1);
    if orient_s1 == 0 {
        return Some(seg1);
    }
    let orient_s2 = orientation_index(line1, line2, seg2);
    if orient_s2 == 0 {
        return Some(seg2);
    }
    if (orient_s1 > 0 && orient_s2 > 0) || (orient_s1 < 0 && orient_s2 < 0) {
        return None;
    }
    if let Some(p) = intersection(line1, line2, seg1, seg2) {
        return Some(p);
    }
    let dist1 = point_to_line_perpendicular(seg1, line1, line2);
    let dist2 = point_to_line_perpendicular(seg2, line1, line2);
    Some(if dist1 < dist2 { seg1 } else { seg2 })
}

/// JTS `Coordinate.distance`.
#[inline]
pub fn distance(a: Coord<f64>, b: Coord<f64>) -> f64 {
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    (dx * dx + dy * dy).sqrt()
}

/// JTS `Distance.pointToSegment`.
pub fn point_to_segment(p: Coord<f64>, a: Coord<f64>, b: Coord<f64>) -> f64 {
    if a.x == b.x && a.y == b.y {
        return distance(p, a);
    }
    let len2 = (b.x - a.x) * (b.x - a.x) + (b.y - a.y) * (b.y - a.y);
    let r = ((p.x - a.x) * (b.x - a.x) + (p.y - a.y) * (b.y - a.y)) / len2;
    if r <= 0.0 {
        return distance(p, a);
    }
    if r >= 1.0 {
        return distance(p, b);
    }
    let s = ((a.y - p.y) * (b.x - a.x) - (a.x - p.x) * (b.y - a.y)) / len2;
    s.abs() * len2.sqrt()
}

/// JTS `Distance.pointToSegmentString`.
pub fn point_to_segment_string(p: Coord<f64>, line: &[Coord<f64>]) -> f64 {
    let mut min_distance = distance(p, line[0]);
    for w in line.windows(2) {
        let dist = point_to_segment(p, w[0], w[1]);
        if dist < min_distance {
            min_distance = dist;
        }
    }
    min_distance
}

/// JTS `Distance.pointToLinePerpendicular`.
pub fn point_to_line_perpendicular(p: Coord<f64>, a: Coord<f64>, b: Coord<f64>) -> f64 {
    let len2 = (b.x - a.x) * (b.x - a.x) + (b.y - a.y) * (b.y - a.y);
    let s = ((a.y - p.y) * (b.x - a.x) - (a.x - p.x) * (b.y - a.y)) / len2;
    s.abs() * len2.sqrt()
}

/// JTS `Envelope.intersects(p1, p2, q)`: whether `q` is in the envelope of `p1-p2`.
#[inline]
pub fn envelope_contains(p1: Coord<f64>, p2: Coord<f64>, q: Coord<f64>) -> bool {
    q.x >= p1.x.min(p2.x) && q.x <= p1.x.max(p2.x) && q.y >= p1.y.min(p2.y) && q.y <= p1.y.max(p2.y)
}

/// JTS `Envelope.intersects(p1, p2, q1, q2)`: whether the envelopes of two segments intersect.
#[inline]
pub fn envelopes_intersect(p1: Coord<f64>, p2: Coord<f64>, q1: Coord<f64>, q2: Coord<f64>) -> bool {
    if p1.x.min(p2.x) > q1.x.max(q2.x) || p1.x.max(p2.x) < q1.x.min(q2.x) {
        return false;
    }
    !(p1.y.min(p2.y) > q1.y.max(q2.y) || p1.y.max(p2.y) < q1.y.min(q2.y))
}

pub const NO_INTERSECTION: i32 = 0;
pub const POINT_INTERSECTION: i32 = 1;
pub const COLLINEAR_INTERSECTION: i32 = 2;

/// JTS `RobustLineIntersector` for segment/segment intersections (2D, floating precision model).
#[derive(Clone, Debug, Default)]
pub struct LineIntersector {
    result: i32,
    int_pt: [Coord<f64>; 2],
    input: [[Coord<f64>; 2]; 2],
    is_proper: bool,
}

impl LineIntersector {
    pub fn new() -> LineIntersector {
        LineIntersector::default()
    }

    pub fn compute_intersection(&mut self, p1: Coord<f64>, p2: Coord<f64>, p3: Coord<f64>, p4: Coord<f64>) {
        self.input = [[p1, p2], [p3, p4]];
        self.result = self.compute_intersect(p1, p2, p3, p4);
    }

    #[inline]
    pub fn has_intersection(&self) -> bool {
        self.result != NO_INTERSECTION
    }

    #[inline]
    pub fn intersection_num(&self) -> i32 {
        self.result
    }

    #[inline]
    pub fn intersection(&self, i: usize) -> Coord<f64> {
        self.int_pt[i]
    }

    #[inline]
    pub fn is_proper(&self) -> bool {
        self.has_intersection() && self.is_proper
    }

    /// JTS `LineIntersector.isInteriorIntersection()`: whether some intersection point is not an endpoint of either
    /// input segment.
    pub fn is_interior_intersection(&self) -> bool {
        (0..2).any(|input| self.is_interior_intersection_of(input))
    }

    /// JTS `LineIntersector.isInteriorIntersection(inputLineIndex)`.
    pub fn is_interior_intersection_of(&self, input: usize) -> bool {
        (0..self.result as usize).any(|i| {
            let p = self.int_pt[i];
            !(p == self.input[input][0] || p == self.input[input][1])
        })
    }

    fn compute_intersect(&mut self, p1: Coord<f64>, p2: Coord<f64>, q1: Coord<f64>, q2: Coord<f64>) -> i32 {
        self.is_proper = false;
        if !envelopes_intersect(p1, p2, q1, q2) {
            return NO_INTERSECTION;
        }
        let pq1 = orientation_index(p1, p2, q1);
        let pq2 = orientation_index(p1, p2, q2);
        if (pq1 > 0 && pq2 > 0) || (pq1 < 0 && pq2 < 0) {
            return NO_INTERSECTION;
        }
        let qp1 = orientation_index(q1, q2, p1);
        let qp2 = orientation_index(q1, q2, p2);
        if (qp1 > 0 && qp2 > 0) || (qp1 < 0 && qp2 < 0) {
            return NO_INTERSECTION;
        }
        if pq1 == 0 && pq2 == 0 && qp1 == 0 && qp2 == 0 {
            return self.compute_collinear_intersection(p1, p2, q1, q2);
        }
        let p = if pq1 == 0 || pq2 == 0 || qp1 == 0 || qp2 == 0 {
            self.is_proper = false;
            if p1 == q1 || p1 == q2 {
                p1
            } else if p2 == q1 || p2 == q2 {
                p2
            } else if pq1 == 0 {
                q1
            } else if pq2 == 0 {
                q2
            } else if qp1 == 0 {
                p1
            } else {
                p2
            }
        } else {
            self.is_proper = true;
            self.proper_intersection(p1, p2, q1, q2)
        };
        self.int_pt[0] = p;
        POINT_INTERSECTION
    }

    fn compute_collinear_intersection(
        &mut self,
        p1: Coord<f64>,
        p2: Coord<f64>,
        q1: Coord<f64>,
        q2: Coord<f64>,
    ) -> i32 {
        let q1in_p = envelope_contains(p1, p2, q1);
        let q2in_p = envelope_contains(p1, p2, q2);
        let p1in_q = envelope_contains(q1, q2, p1);
        let p2in_q = envelope_contains(q1, q2, p2);
        let mut set = |a: Coord<f64>, b: Coord<f64>, point: bool| {
            self.int_pt = [a, b];
            if point { POINT_INTERSECTION } else { COLLINEAR_INTERSECTION }
        };
        if q1in_p && q2in_p {
            return set(q1, q2, false);
        }
        if p1in_q && p2in_q {
            return set(p1, p2, false);
        }
        if q1in_p && p1in_q {
            return set(q1, p1, q1 == p1 && !q2in_p && !p2in_q);
        }
        if q1in_p && p2in_q {
            return set(q1, p2, q1 == p2 && !q2in_p && !p1in_q);
        }
        if q2in_p && p1in_q {
            return set(q2, p1, q2 == p1 && !q1in_p && !p2in_q);
        }
        if q2in_p && p2in_q {
            return set(q2, p2, q2 == p2 && !q1in_p && !p1in_q);
        }
        NO_INTERSECTION
    }

    fn proper_intersection(&self, p1: Coord<f64>, p2: Coord<f64>, q1: Coord<f64>, q2: Coord<f64>) -> Coord<f64> {
        let int_pt = intersection(p1, p2, q1, q2).unwrap_or_else(|| nearest_endpoint(p1, p2, q1, q2));
        if !self.is_in_segment_envelopes(int_pt) { nearest_endpoint(p1, p2, q1, q2) } else { int_pt }
    }

    fn is_in_segment_envelopes(&self, p: Coord<f64>) -> bool {
        envelope_contains(self.input[0][0], self.input[0][1], p)
            && envelope_contains(self.input[1][0], self.input[1][1], p)
    }
}

fn nearest_endpoint(p1: Coord<f64>, p2: Coord<f64>, q1: Coord<f64>, q2: Coord<f64>) -> Coord<f64> {
    let mut nearest = p1;
    let mut min_dist = point_to_segment(p1, q1, q2);
    for (pt, dist) in
        [(p2, point_to_segment(p2, q1, q2)), (q1, point_to_segment(q1, p1, p2)), (q2, point_to_segment(q2, p1, p2))]
    {
        if dist < min_dist {
            min_dist = dist;
            nearest = pt;
        }
    }
    nearest
}

/// Java `Math.atan2` / `StrictMath.atan2` (fdlibm). `f64::atan2` uses the platform libm, which can differ in the last
/// bit.
#[inline]
pub fn atan2(y: f64, x: f64) -> f64 {
    libm::atan2(y, x)
}

/// Java `Math.sin` (fdlibm, like `StrictMath.sin`).
#[inline]
pub fn sin(x: f64) -> f64 {
    libm::sin(x)
}

/// Java `Math.cos` (fdlibm, like `StrictMath.cos`).
#[inline]
pub fn cos(x: f64) -> f64 {
    libm::cos(x)
}

pub mod angle {
    use super::{Coord, atan2, cos, sin};
    use std::f64::consts::PI;

    pub const PI_TIMES_2: f64 = 2.0 * PI;
    pub const PI_OVER_2: f64 = PI / 2.0;

    /// JTS `Angle.angle(p0, p1)`.
    #[inline]
    pub fn angle(p0: Coord<f64>, p1: Coord<f64>) -> f64 {
        atan2(p1.y - p0.y, p1.x - p0.x)
    }

    /// JTS `Angle.angleBetweenOriented`.
    pub fn angle_between_oriented(tip1: Coord<f64>, tail: Coord<f64>, tip2: Coord<f64>) -> f64 {
        let a1 = angle(tail, tip1);
        let a2 = angle(tail, tip2);
        let ang_del = a2 - a1;
        if ang_del <= -PI {
            return ang_del + PI_TIMES_2;
        }
        if ang_del > PI {
            return ang_del - PI_TIMES_2;
        }
        ang_del
    }

    /// JTS `Angle.normalize`.
    pub fn normalize(mut angle: f64) -> f64 {
        while angle > PI {
            angle -= PI_TIMES_2;
        }
        while angle <= -PI {
            angle += PI_TIMES_2;
        }
        angle
    }

    #[inline]
    pub fn sin_snap(ang: f64) -> f64 {
        let res = sin(ang);
        if res.abs() < 5e-16 { 0.0 } else { res }
    }

    #[inline]
    pub fn cos_snap(ang: f64) -> f64 {
        let res = cos(ang);
        if res.abs() < 5e-16 { 0.0 } else { res }
    }
}

/// JTS `Area.ofRingSigned` (positive if the ring is clockwise).
pub fn signed_ring_area(ring: &[Coord<f64>]) -> f64 {
    if ring.len() < 3 {
        return 0.0;
    }
    let mut sum = 0.0;
    let x0 = ring[0].x;
    for i in 1..ring.len() - 1 {
        let x = ring[i].x - x0;
        let y1 = ring[i + 1].y;
        let y2 = ring[i - 1].y;
        sum += x * (y2 - y1);
    }
    sum / 2.0
}

/// JTS `Orientation.isCCWArea`.
#[inline]
pub fn is_ccw_area(ring: &[Coord<f64>]) -> bool {
    signed_ring_area(ring) < 0.0
}

/// JTS `CoordinateArrays.isRing`.
#[inline]
pub fn is_ring(pts: &[Coord<f64>]) -> bool {
    pts.len() >= 4 && pts[0] == pts[pts.len() - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(x: f64, y: f64) -> Coord<f64> {
        Coord { x, y }
    }

    #[test]
    fn orientation() {
        assert_eq!(1, orientation_index(c(0.0, 0.0), c(1.0, 0.0), c(0.5, 1.0)));
        assert_eq!(-1, orientation_index(c(0.0, 0.0), c(1.0, 0.0), c(0.5, -1.0)));
        assert_eq!(0, orientation_index(c(0.0, 0.0), c(1.0, 1.0), c(2.0, 2.0)));
        // needs the DD fallback
        assert_eq!(0, orientation_index(c(0.1, 0.1), c(0.3, 0.3), c(0.7, 0.7)));
    }

    #[test]
    fn dd_intersection() {
        assert_eq!(Some(c(0.5, 0.5)), intersection(c(0.0, 0.0), c(1.0, 1.0), c(0.0, 1.0), c(1.0, 0.0)));
        assert_eq!(None, intersection(c(0.0, 0.0), c(1.0, 1.0), c(0.0, 1.0), c(1.0, 2.0)));
    }

    #[test]
    fn line_intersector() {
        let mut li = LineIntersector::new();
        li.compute_intersection(c(0.0, 0.0), c(2.0, 2.0), c(0.0, 2.0), c(2.0, 0.0));
        assert!(li.is_proper());
        assert_eq!(c(1.0, 1.0), li.intersection(0));
        li.compute_intersection(c(0.0, 0.0), c(2.0, 0.0), c(1.0, 0.0), c(3.0, 0.0));
        assert_eq!(COLLINEAR_INTERSECTION, li.intersection_num());
        li.compute_intersection(c(0.0, 0.0), c(1.0, 0.0), c(1.0, 0.0), c(2.0, 0.0));
        assert_eq!(POINT_INTERSECTION, li.intersection_num());
        li.compute_intersection(c(0.0, 0.0), c(1.0, 0.0), c(0.0, 1.0), c(1.0, 1.0));
        assert!(!li.has_intersection());
    }

    #[test]
    fn distances() {
        assert_eq!(1.0, point_to_segment(c(0.5, 1.0), c(0.0, 0.0), c(1.0, 0.0)));
        assert_eq!(2f64.sqrt(), point_to_segment(c(2.0, 1.0), c(0.0, 0.0), c(1.0, 0.0)));
    }
}

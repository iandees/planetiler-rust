//! Port of the semantics of JTS `IsValidOp`: OGC validity checks for geometries.
//!
//! Unlike `geo::Validation`, this follows JTS rules exactly where planetiler depends on them (rings may not
//! self-touch, holes may touch the shell or each other at single points as long as the interior stays connected,
//! repeated points are allowed) and uses a sweep over segment envelopes so that it is fast enough to run on every
//! polygon in every tile.

use geo::kernels::{Kernel, Orientation, RobustKernel};
use geo_types::{Coord, Geometry, LineString, Polygon};

/// The reason a geometry is invalid (subset of JTS `TopologyValidationError` codes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValidationError {
    InvalidCoordinate,
    TooFewPoints,
    RingNotClosed,
    SelfIntersection,
    RingSelfIntersection,
    HoleOutsideShell,
    NestedHoles,
    DisconnectedInterior,
    NestedShells,
}

/// Returns true if `geom` is valid according to JTS / OGC rules.
pub fn is_valid(geom: &Geometry<f64>) -> bool {
    validation_error(geom).is_none()
}

/// Returns the first validation problem found in `geom`, or `None` if it is valid.
pub fn validation_error(geom: &Geometry<f64>) -> Option<ValidationError> {
    match geom {
        Geometry::Point(p) => check_coord(&p.0),
        Geometry::MultiPoint(mp) => mp.0.iter().find_map(|p| check_coord(&p.0)),
        Geometry::Line(l) => check_line(&LineString::from(*l)),
        Geometry::LineString(ls) => check_line(ls),
        Geometry::MultiLineString(mls) => mls.0.iter().find_map(check_line),
        Geometry::Polygon(p) => polygonal_error(std::slice::from_ref(p)),
        Geometry::MultiPolygon(mp) => polygonal_error(&mp.0),
        Geometry::Rect(r) => polygonal_error(&[r.to_polygon()]),
        Geometry::Triangle(t) => polygonal_error(&[t.to_polygon()]),
        Geometry::GeometryCollection(gc) => gc.0.iter().find_map(validation_error),
    }
}

/// Returns true if every polygon in `polys` is valid and they form a valid multipolygon.
pub fn is_valid_polygons(polys: &[Polygon<f64>]) -> bool {
    polygonal_error(polys).is_none()
}

fn check_coord(c: &Coord<f64>) -> Option<ValidationError> {
    if c.x.is_finite() && c.y.is_finite() { None } else { Some(ValidationError::InvalidCoordinate) }
}

fn check_line(ls: &LineString<f64>) -> Option<ValidationError> {
    if ls.0.is_empty() {
        return None;
    }
    if let Some(e) = ls.0.iter().find_map(check_coord) {
        return Some(e);
    }
    if ls.0.iter().all(|c| *c == ls.0[0]) {
        return Some(ValidationError::TooFewPoints);
    }
    None
}

#[inline]
fn orient(p: Coord<f64>, q: Coord<f64>, r: Coord<f64>) -> i8 {
    match RobustKernel::orient2d(p, q, r) {
        Orientation::CounterClockwise => 1,
        Orientation::Clockwise => -1,
        Orientation::Collinear => 0,
    }
}

/// JTS `PointLocation` result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Location {
    Interior,
    Boundary,
    Exterior,
}

/// Port of JTS `RayCrossingCounter.locatePointInRing`. `ring` must be closed.
pub fn locate_in_ring(p: Coord<f64>, ring: &[Coord<f64>]) -> Location {
    let mut crossings = 0usize;
    for w in ring.windows(2) {
        let (p1, p2) = (w[0], w[1]);
        if p1.x < p.x && p2.x < p.x {
            continue;
        }
        if p.x == p2.x && p.y == p2.y {
            return Location::Boundary;
        }
        if p1.y == p.y && p2.y == p.y {
            let (minx, maxx) = if p1.x > p2.x { (p2.x, p1.x) } else { (p1.x, p2.x) };
            if p.x >= minx && p.x <= maxx {
                return Location::Boundary;
            }
            continue;
        }
        if (p1.y > p.y && p2.y <= p.y) || (p2.y > p.y && p1.y <= p.y) {
            let mut o = orient(p1, p2, p);
            if o == 0 {
                return Location::Boundary;
            }
            if p2.y < p1.y {
                o = -o;
            }
            if o == 1 {
                crossings += 1;
            }
        }
    }
    if crossings % 2 == 1 { Location::Interior } else { Location::Exterior }
}

/// Location of `p` relative to a polygon (shell minus holes), using deduplicated rings.
fn locate_in_polygon(p: Coord<f64>, rings: &[Ring]) -> Location {
    match locate_in_ring(p, &rings[0].pts) {
        Location::Interior => {}
        other => return other,
    }
    for hole in &rings[1..] {
        if !hole.env_contains(p) {
            continue;
        }
        match locate_in_ring(p, &hole.pts) {
            Location::Interior => return Location::Exterior,
            Location::Boundary => return Location::Boundary,
            Location::Exterior => {}
        }
    }
    Location::Interior
}

struct Ring {
    /// closed ring with consecutive duplicate points removed
    pts: Vec<Coord<f64>>,
    poly: usize,
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

impl Ring {
    fn new(src: &[Coord<f64>], poly: usize) -> Ring {
        let mut pts: Vec<Coord<f64>> = Vec::with_capacity(src.len());
        let (mut min_x, mut min_y, mut max_x, mut max_y) =
            (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        for c in src {
            if pts.last() != Some(c) {
                pts.push(*c);
                min_x = min_x.min(c.x);
                min_y = min_y.min(c.y);
                max_x = max_x.max(c.x);
                max_y = max_y.max(c.y);
            }
        }
        Ring { pts, poly, min_x, min_y, max_x, max_y }
    }

    fn env_contains(&self, p: Coord<f64>) -> bool {
        p.x >= self.min_x && p.x <= self.max_x && p.y >= self.min_y && p.y <= self.max_y
    }

    fn env_covers(&self, other: &Ring) -> bool {
        other.min_x >= self.min_x && other.max_x <= self.max_x && other.min_y >= self.min_y && other.max_y <= self.max_y
    }

    fn env_intersects(&self, other: &Ring) -> bool {
        other.min_x <= self.max_x && other.max_x >= self.min_x && other.min_y <= self.max_y && other.max_y >= self.min_y
    }

    fn num_segments(&self) -> usize {
        self.pts.len() - 1
    }

    /// Candidate points of this ring to test for nesting: vertices then segment midpoints.
    fn test_points(&self) -> impl Iterator<Item = Coord<f64>> + '_ {
        let n = self.num_segments();
        self.pts[..n]
            .iter()
            .copied()
            .chain(self.pts.windows(2).map(|w| Coord { x: (w[0].x + w[1].x) / 2.0, y: (w[0].y + w[1].y) / 2.0 }))
    }
}

#[derive(Clone, Copy)]
struct Seg {
    ring: u32,
    idx: u32,
    min_x: f64,
    max_x: f64,
    min_y: f64,
    max_y: f64,
}

/// A point where two different rings touch.
struct Touch {
    pt: Coord<f64>,
    ring_a: u32,
    seg_a: u32,
    ring_b: u32,
    seg_b: u32,
}

enum SegIntersection {
    None,
    /// proper crossing or collinear overlap
    Invalid,
    Touch(Coord<f64>),
}

fn intersect(a0: Coord<f64>, a1: Coord<f64>, b0: Coord<f64>, b1: Coord<f64>) -> SegIntersection {
    let o1 = orient(a0, a1, b0);
    let o2 = orient(a0, a1, b1);
    if o1 != 0 && o1 == o2 {
        return SegIntersection::None;
    }
    let o3 = orient(b0, b1, a0);
    let o4 = orient(b0, b1, a1);
    if o3 != 0 && o3 == o4 {
        return SegIntersection::None;
    }
    if o1 == 0 && o2 == 0 {
        // collinear: project onto the dominant axis
        let use_x = (a1.x - a0.x).abs() >= (a1.y - a0.y).abs();
        let key = |c: Coord<f64>| if use_x { c.x } else { c.y };
        let (amin, amax) = (key(a0).min(key(a1)), key(a0).max(key(a1)));
        let (bmin, bmax) = (key(b0).min(key(b1)), key(b0).max(key(b1)));
        let lo = amin.max(bmin);
        let hi = amax.min(bmax);
        if lo > hi {
            return SegIntersection::None;
        }
        if lo < hi {
            return SegIntersection::Invalid;
        }
        // touch at a single endpoint
        let pt = [a0, a1].into_iter().find(|c| key(*c) == lo && (*c == b0 || *c == b1));
        return match pt {
            Some(p) => SegIntersection::Touch(p),
            None => SegIntersection::None,
        };
    }
    if o1 != 0 && o2 != 0 && o3 != 0 && o4 != 0 {
        return SegIntersection::Invalid;
    }
    let pt = if o1 == 0 {
        b0
    } else if o2 == 0 {
        b1
    } else if o3 == 0 {
        a0
    } else {
        a1
    };
    SegIntersection::Touch(pt)
}

fn quadrant(origin: Coord<f64>, p: Coord<f64>) -> i8 {
    let dx = p.x - origin.x;
    let dy = p.y - origin.y;
    if dx >= 0.0 {
        if dy >= 0.0 { 0 } else { 3 }
    } else if dy >= 0.0 {
        1
    } else {
        2
    }
}

fn compare_angle(origin: Coord<f64>, p: Coord<f64>, q: Coord<f64>) -> i8 {
    let (qp, qq) = (quadrant(origin, p), quadrant(origin, q));
    if qp > qq {
        return 1;
    }
    if qp < qq {
        return -1;
    }
    orient(origin, q, p)
}

fn is_angle_greater(origin: Coord<f64>, p: Coord<f64>, q: Coord<f64>) -> bool {
    let (qp, qq) = (quadrant(origin, p), quadrant(origin, q));
    if qp > qq {
        return true;
    }
    if qp < qq {
        return false;
    }
    orient(origin, q, p) == 1
}

fn compare_between(origin: Coord<f64>, p: Coord<f64>, e0: Coord<f64>, e1: Coord<f64>) -> i8 {
    let c0 = compare_angle(origin, p, e0);
    if c0 == 0 {
        return 0;
    }
    let c1 = compare_angle(origin, p, e1);
    if c1 == 0 {
        return 0;
    }
    if c0 > 0 && c1 < 0 { 1 } else { -1 }
}

/// Port of JTS `PolygonNodeTopology.isCrossing`.
fn is_crossing(node: Coord<f64>, a0: Coord<f64>, a1: Coord<f64>, b0: Coord<f64>, b1: Coord<f64>) -> bool {
    let (mut lo, mut hi) = (a0, a1);
    if is_angle_greater(node, lo, hi) {
        std::mem::swap(&mut lo, &mut hi);
    }
    let c0 = compare_between(node, b0, lo, hi);
    if c0 == 0 {
        return false;
    }
    let c1 = compare_between(node, b1, lo, hi);
    if c1 == 0 {
        return false;
    }
    c0 != c1
}

/// The 2 points adjacent to `pt` along `ring` where `pt` lies on segment `seg`.
fn neighbors(ring: &Ring, seg: usize, pt: Coord<f64>) -> (Coord<f64>, Coord<f64>) {
    let n = ring.num_segments();
    let (p0, p1) = (ring.pts[seg], ring.pts[seg + 1]);
    if pt == p0 {
        (ring.pts[(seg + n - 1) % n], p1)
    } else if pt == p1 {
        (p0, ring.pts[(seg + 2) % n])
    } else {
        (p0, p1)
    }
}

/// Union-find over ring and touch-point nodes for detecting disconnected interiors.
struct UnionFind(Vec<usize>);

impl UnionFind {
    fn find(&mut self, mut x: usize) -> usize {
        while self.0[x] != x {
            self.0[x] = self.0[self.0[x]];
            x = self.0[x];
        }
        x
    }

    /// Returns false if `a` and `b` were already connected.
    fn union(&mut self, a: usize, b: usize) -> bool {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return false;
        }
        self.0[ra] = rb;
        true
    }
}

fn polygonal_error(polys: &[Polygon<f64>]) -> Option<ValidationError> {
    // build deduplicated rings; rings[poly_start[i]] is the shell of polygon i
    let mut rings: Vec<Ring> = Vec::new();
    let mut poly_rings: Vec<(usize, usize)> = Vec::with_capacity(polys.len());
    for (pi, poly) in polys.iter().enumerate() {
        if poly.exterior().0.is_empty() {
            continue;
        }
        let start = rings.len();
        for ring in std::iter::once(poly.exterior()).chain(poly.interiors()) {
            if ring.0.is_empty() {
                continue;
            }
            if let Some(e) = ring.0.iter().find_map(check_coord) {
                return Some(e);
            }
            if ring.0.first() != ring.0.last() {
                return Some(ValidationError::RingNotClosed);
            }
            let r = Ring::new(&ring.0, pi);
            if r.pts.len() < 4 {
                return Some(ValidationError::TooFewPoints);
            }
            rings.push(r);
        }
        poly_rings.push((start, rings.len()));
    }
    if rings.is_empty() {
        return None;
    }

    // segment intersection sweep
    let mut segs: Vec<Seg> = Vec::with_capacity(rings.iter().map(|r| r.num_segments()).sum());
    for (ri, ring) in rings.iter().enumerate() {
        for (si, w) in ring.pts.windows(2).enumerate() {
            segs.push(Seg {
                ring: ri as u32,
                idx: si as u32,
                min_x: w[0].x.min(w[1].x),
                max_x: w[0].x.max(w[1].x),
                min_y: w[0].y.min(w[1].y),
                max_y: w[0].y.max(w[1].y),
            });
        }
    }
    segs.sort_unstable_by(|a, b| a.min_x.total_cmp(&b.min_x));
    let mut touches: Vec<Touch> = Vec::new();
    for i in 0..segs.len() {
        let a = segs[i];
        let ring_a = &rings[a.ring as usize];
        let (a0, a1) = (ring_a.pts[a.idx as usize], ring_a.pts[a.idx as usize + 1]);
        for b in &segs[i + 1..] {
            if b.min_x > a.max_x {
                break;
            }
            if b.min_y > a.max_y || b.max_y < a.min_y {
                continue;
            }
            let ring_b = &rings[b.ring as usize];
            let (b0, b1) = (ring_b.pts[b.idx as usize], ring_b.pts[b.idx as usize + 1]);
            match intersect(a0, a1, b0, b1) {
                SegIntersection::None => {}
                SegIntersection::Invalid => return Some(ValidationError::SelfIntersection),
                SegIntersection::Touch(pt) => {
                    if a.ring == b.ring {
                        let n = ring_a.num_segments() as u32;
                        let (lo, hi) = if a.idx < b.idx { (a.idx, b.idx) } else { (b.idx, a.idx) };
                        let shared = if hi == lo + 1 {
                            Some(ring_a.pts[hi as usize])
                        } else if lo == 0 && hi == n - 1 {
                            Some(ring_a.pts[0])
                        } else {
                            None
                        };
                        if shared != Some(pt) {
                            return Some(ValidationError::RingSelfIntersection);
                        }
                    } else {
                        touches.push(Touch { pt, ring_a: a.ring, seg_a: a.idx, ring_b: b.ring, seg_b: b.idx });
                    }
                }
            }
        }
    }

    // check that rings touching at a point do not cross there
    for t in &touches {
        let ra = &rings[t.ring_a as usize];
        let rb = &rings[t.ring_b as usize];
        let (a0, a1) = neighbors(ra, t.seg_a as usize, t.pt);
        let (b0, b1) = neighbors(rb, t.seg_b as usize, t.pt);
        if is_crossing(t.pt, a0, a1, b0, b1) {
            return Some(ValidationError::SelfIntersection);
        }
    }

    // interior connectivity: a cycle in the ring <-> touch point graph within a polygon disconnects the interior
    if !touches.is_empty() {
        let mut edges: Vec<(u32, u64, u64)> = touches
            .iter()
            .filter(|t| rings[t.ring_a as usize].poly == rings[t.ring_b as usize].poly)
            .flat_map(|t| {
                let (x, y) = (norm_bits(t.pt.x), norm_bits(t.pt.y));
                [(t.ring_a, x, y), (t.ring_b, x, y)]
            })
            .collect();
        edges.sort_unstable();
        edges.dedup();
        let mut point_ids: hashbrown::HashMap<(u64, u64), usize> = hashbrown::HashMap::new();
        let mut uf = UnionFind((0..rings.len()).collect());
        for (ring, x, y) in edges {
            let next_id = rings.len() + point_ids.len();
            let id = *point_ids.entry((x, y)).or_insert(next_id);
            if id >= uf.0.len() {
                uf.0.push(id);
            }
            if !uf.union(ring as usize, id) {
                return Some(ValidationError::DisconnectedInterior);
            }
        }
    }

    // holes must be inside their shell and not nested within each other
    for &(start, end) in &poly_rings {
        let shell = &rings[start];
        for hole in &rings[start + 1..end] {
            if !shell.env_covers(hole) || !is_ring_inside(hole, shell) {
                return Some(ValidationError::HoleOutsideShell);
            }
        }
        if end - start > 2 && has_nested_rings(&rings[start + 1..end]) {
            return Some(ValidationError::NestedHoles);
        }
    }

    // shells of a multipolygon must not be nested
    if poly_rings.len() > 1 {
        let mut order: Vec<usize> = (0..poly_rings.len()).collect();
        order.sort_unstable_by(|a, b| rings[poly_rings[*a].0].min_x.total_cmp(&rings[poly_rings[*b].0].min_x));
        for (oi, &i) in order.iter().enumerate() {
            let shell_i = &rings[poly_rings[i].0];
            for &j in &order[oi + 1..] {
                let shell_j = &rings[poly_rings[j].0];
                if shell_j.min_x > shell_i.max_x {
                    break;
                }
                if !shell_i.env_intersects(shell_j) {
                    continue;
                }
                let (pj, pi) = (poly_rings[j], poly_rings[i]);
                if is_shell_nested(shell_i, &rings[pj.0..pj.1]) || is_shell_nested(shell_j, &rings[pi.0..pi.1]) {
                    return Some(ValidationError::NestedShells);
                }
            }
        }
    }

    None
}

#[inline]
fn norm_bits(v: f64) -> u64 {
    if v == 0.0 { 0 } else { v.to_bits() }
}

/// True if the first point of `test` that is not on the boundary of `target` is inside `target`.
fn is_ring_inside(test: &Ring, target: &Ring) -> bool {
    for p in test.test_points() {
        match locate_in_ring(p, &target.pts) {
            Location::Interior => return true,
            Location::Exterior => return false,
            Location::Boundary => {}
        }
    }
    false
}

fn has_nested_rings(holes: &[Ring]) -> bool {
    let mut order: Vec<usize> = (0..holes.len()).collect();
    order.sort_unstable_by(|a, b| holes[*a].min_x.total_cmp(&holes[*b].min_x));
    for (oi, &i) in order.iter().enumerate() {
        for &j in &order[oi + 1..] {
            if holes[j].min_x > holes[i].max_x {
                break;
            }
            if !holes[i].env_intersects(&holes[j]) {
                continue;
            }
            if (holes[j].env_covers(&holes[i]) && is_ring_inside(&holes[i], &holes[j]))
                || (holes[i].env_covers(&holes[j]) && is_ring_inside(&holes[j], &holes[i]))
            {
                return true;
            }
        }
    }
    false
}

/// True if `shell` is inside the polygon formed by `outer` (shell followed by holes).
fn is_shell_nested(shell: &Ring, outer: &[Ring]) -> bool {
    if !outer[0].env_covers(shell) {
        return false;
    }
    for p in shell.test_points() {
        match locate_in_polygon(p, outer) {
            Location::Interior => return true,
            Location::Exterior => return false,
            Location::Boundary => {}
        }
    }
    false
}

/// JTS `IsSimpleOp` for polygonal geometries: every ring (checked on its own) only touches itself at its endpoints.
pub fn is_simple_polygonal(g: &Geometry<f64>) -> bool {
    let rings: Vec<&LineString<f64>> = match g {
        Geometry::Polygon(p) => std::iter::once(p.exterior()).chain(p.interiors()).collect(),
        Geometry::MultiPolygon(mp) => {
            mp.0.iter().flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors())).collect()
        }
        _ => return true,
    };
    rings.into_iter().all(is_simple_ring)
}

fn is_simple_ring(ring: &LineString<f64>) -> bool {
    use geo::line_intersection::{LineIntersection, line_intersection};
    use rstar::primitives::{GeomWithData, Rectangle};

    let mut pts: Vec<Coord<f64>> = ring.0.clone();
    pts.dedup();
    if pts.len() < 4 {
        return true;
    }
    let segments: Vec<geo_types::Line<f64>> = pts.windows(2).map(|w| geo_types::Line::new(w[0], w[1])).collect();
    let n = segments.len();
    let closed = pts.first() == pts.last();
    let tree = rstar::RTree::bulk_load(
        segments
            .iter()
            .enumerate()
            .map(|(i, s)| GeomWithData::new(Rectangle::from_corners([s.start.x, s.start.y], [s.end.x, s.end.y]), i))
            .collect(),
    );
    for (i, a) in segments.iter().enumerate() {
        let envelope = rstar::AABB::from_corners([a.start.x, a.start.y], [a.end.x, a.end.y]);
        for other in tree.locate_in_envelope_intersecting(envelope) {
            let j = other.data;
            if j <= i {
                continue;
            }
            let Some(intersection) = line_intersection(*a, segments[j]) else { continue };
            let adjacent = j == i + 1 || (closed && i == 0 && j == n - 1);
            match intersection {
                LineIntersection::Collinear { .. } => return false,
                LineIntersection::SinglePoint { intersection, .. } => {
                    if !adjacent {
                        return false;
                    }
                    // adjacent segments may only share their common vertex
                    let shared = if j == i + 1 { a.end } else { a.start };
                    if intersection != shared {
                        return false;
                    }
                }
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::test_utils::*;

    fn poly(coords: &[f64]) -> Geometry<f64> {
        new_polygon(coords).into()
    }

    #[test]
    fn simple_valid() {
        assert!(is_valid(&rectangle(0.0, 1.0).into()));
        assert!(is_valid(&new_point(0.0, 1.0).into()));
        assert!(is_valid(&new_line_string(&[0.0, 0.0, 1.0, 1.0]).into()));
        assert!(!is_valid(&new_line_string(&[0.0, 0.0, 0.0, 0.0]).into()));
        assert!(is_valid(&empty_geometry()));
        // repeated points are ok
        assert!(is_valid(&poly(&[0.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 0.0])));
    }

    #[test]
    fn bowtie() {
        assert_eq!(
            Some(ValidationError::SelfIntersection),
            validation_error(&poly(&[0.0, 0.0, 1.0, 1.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0]))
        );
    }

    #[test]
    fn spike() {
        assert!(!is_valid(&poly(&[0.0, 0.0, 2.0, 0.0, 3.0, 0.0, 2.0, 0.0, 2.0, 2.0, 0.0, 2.0, 0.0, 0.0])));
    }

    #[test]
    fn self_touching_ring() {
        // ring touches itself at (1, 0)
        assert_eq!(
            Some(ValidationError::RingSelfIntersection),
            validation_error(&poly(&[0.0, 0.0, 1.0, 0.0, 2.0, 0.0, 2.0, 2.0, 1.0, 0.0, 0.0, 2.0, 0.0, 0.0]))
        );
    }

    #[test]
    fn collapsed() {
        assert!(!is_valid(&poly(&[0.0, 0.0, 1.0, 0.0, 2.0, 0.0, 0.0, 0.0])));
        assert!(!is_valid(&poly(&[0.0, 0.0, 1.0, 0.0, 0.0, 0.0])));
    }

    #[test]
    fn holes() {
        let shell = rectangle_coord_list(0.0, 10.0);
        let valid = new_polygon_with_holes(shell.clone(), vec![rectangle_coord_list(1.0, 2.0)]);
        assert!(is_valid(&valid.into()));
        // hole touching shell at one point
        let touching =
            new_polygon_with_holes(shell.clone(), vec![new_coordinate_list(&[0.0, 5.0, 2.0, 4.0, 2.0, 6.0, 0.0, 5.0])]);
        assert!(is_valid(&touching.into()));
        // hole touching shell at 2 points disconnects interior
        let disconnected = new_polygon_with_holes(
            shell.clone(),
            vec![new_coordinate_list(&[0.0, 5.0, 5.0, 0.0, 10.0, 5.0, 5.0, 4.0, 0.0, 5.0])],
        );
        assert_eq!(Some(ValidationError::DisconnectedInterior), validation_error(&disconnected.into()));
        // hole outside shell
        let outside = new_polygon_with_holes(shell.clone(), vec![rectangle_coord_list(11.0, 12.0)]);
        assert_eq!(Some(ValidationError::HoleOutsideShell), validation_error(&outside.into()));
        // nested holes
        let nested =
            new_polygon_with_holes(shell.clone(), vec![rectangle_coord_list(1.0, 5.0), rectangle_coord_list(2.0, 3.0)]);
        assert_eq!(Some(ValidationError::NestedHoles), validation_error(&nested.into()));
        // hole sharing an edge with shell
        let shared = new_polygon_with_holes(shell, vec![rectangle_coord_list(0.0, 2.0)]);
        assert!(!is_valid(&shared.into()));
        // two holes touching at one point
        let two = new_polygon_with_holes(
            rectangle_coord_list(0.0, 10.0),
            vec![rectangle_coord_list(1.0, 2.0), rectangle_coord_list(2.0, 3.0)],
        );
        assert!(is_valid(&two.into()));
    }

    #[test]
    fn multipolygons() {
        let touching_point: Geometry<f64> = new_multi_polygon(vec![rectangle(0.0, 1.0), rectangle(1.0, 2.0)]).into();
        assert!(is_valid(&touching_point));
        let touching_edge: Geometry<f64> =
            new_multi_polygon(vec![rectangle(0.0, 1.0), rectangle_xy(1.0, 0.0, 2.0, 1.0)]).into();
        assert!(!is_valid(&touching_edge));
        let nested: Geometry<f64> = new_multi_polygon(vec![rectangle(0.0, 10.0), rectangle(1.0, 2.0)]).into();
        assert_eq!(Some(ValidationError::NestedShells), validation_error(&nested));
        let island_in_hole: Geometry<f64> = new_multi_polygon(vec![
            new_polygon_with_holes(rectangle_coord_list(0.0, 10.0), vec![rectangle_coord_list(1.0, 9.0)]),
            rectangle(2.0, 3.0),
        ])
        .into();
        assert!(is_valid(&island_in_hole));
        let overlapping: Geometry<f64> = new_multi_polygon(vec![rectangle(0.0, 2.0), rectangle(1.0, 3.0)]).into();
        assert!(!is_valid(&overlapping));
    }

    #[test]
    fn locate() {
        let ring = rectangle_coord_list(0.0, 1.0);
        assert_eq!(Location::Interior, locate_in_ring(Coord { x: 0.5, y: 0.5 }, &ring));
        assert_eq!(Location::Boundary, locate_in_ring(Coord { x: 0.0, y: 0.5 }, &ring));
        assert_eq!(Location::Boundary, locate_in_ring(Coord { x: 0.0, y: 0.0 }, &ring));
        assert_eq!(Location::Exterior, locate_in_ring(Coord { x: 1.5, y: 0.5 }, &ring));
    }
}

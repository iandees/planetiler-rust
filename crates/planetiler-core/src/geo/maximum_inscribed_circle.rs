//! Port of JTS `MaximumInscribedCircle` (and `ExactMaxInscribedCircle`): finds the point inside a polygon furthest
//! from its boundary ("pole of inaccessibility"), using the polylabel grid refinement algorithm.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use geo::kernels::{Kernel, Orientation, RobustKernel};
use geo::line_intersection::{LineIntersection, line_intersection};
use geo_types::{Coord, Geometry, Line, Polygon};
use rstar::RTree;
use rstar::primitives::Line as RLine;

use crate::geo::envelope::Envelope;
use crate::geo::interior_point;
use crate::geo::is_valid::{Location, locate_in_ring};

const SQRT2: f64 = std::f64::consts::SQRT_2;
const AUTO_TOLERANCE_FRACTION: f64 = 0.001;

struct Cell {
    x: f64,
    y: f64,
    h_side: f64,
    distance: f64,
    max_dist: f64,
}

impl Cell {
    fn new(x: f64, y: f64, h_side: f64, distance: f64) -> Cell {
        Cell { x, y, h_side, distance, max_dist: distance + h_side * SQRT2 }
    }
}

impl PartialEq for Cell {
    fn eq(&self, other: &Self) -> bool {
        self.max_dist == other.max_dist
    }
}

impl Eq for Cell {}

impl PartialOrd for Cell {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Cell {
    // max-heap on max_dist
    fn cmp(&self, other: &Self) -> Ordering {
        self.max_dist.total_cmp(&other.max_dist)
    }
}

struct Polygonal<'a> {
    polys: Vec<&'a Polygon<f64>>,
    boundary: RTree<RLine<[f64; 2]>>,
}

impl Polygonal<'_> {
    fn locate(&self, p: Coord<f64>) -> Location {
        // JTS IndexedPointInAreaLocator: counts crossings over all rings
        let mut on_boundary = false;
        let mut inside = false;
        for poly in &self.polys {
            for ring in std::iter::once(poly.exterior()).chain(poly.interiors()) {
                match locate_in_ring(p, &ring.0) {
                    Location::Boundary => on_boundary = true,
                    Location::Interior => inside = !inside,
                    Location::Exterior => {}
                }
            }
        }
        if on_boundary {
            Location::Boundary
        } else if inside {
            Location::Interior
        } else {
            Location::Exterior
        }
    }

    fn distance_to_boundary(&self, x: f64, y: f64) -> f64 {
        let dist =
            self.boundary.nearest_neighbor([x, y]).map_or(0.0, |l| rstar::PointDistance::distance_2(l, &[x, y]).sqrt());
        if self.locate(Coord { x, y }) == Location::Exterior { -dist } else { dist }
    }
}

fn collect_polygons<'a>(geom: &'a Geometry<f64>, out: &mut Vec<&'a Polygon<f64>>) {
    match geom {
        Geometry::Polygon(p) if !p.exterior().0.is_empty() => out.push(p),
        Geometry::MultiPolygon(mp) => out.extend(mp.0.iter().filter(|p| !p.exterior().0.is_empty())),
        Geometry::GeometryCollection(gc) => gc.0.iter().for_each(|g| collect_polygons(g, out)),
        _ => {}
    }
}

/// JTS `MaximumInscribedCircle.getCenter(polygonal, tolerance)`. A tolerance of 0 uses an automatic tolerance.
/// Returns `None` for empty or non-polygonal input.
pub fn get_center(polygonal: &Geometry<f64>, tolerance: f64) -> Option<Coord<f64>> {
    let mut polys = Vec::new();
    collect_polygons(polygonal, &mut polys);
    if polys.is_empty() {
        return None;
    }
    if crate::geo::geo_utils::area(polygonal) == 0.0 {
        return Some(polys[0].exterior().0[0]);
    }
    if polys.len() == 1 && polys[0].interiors().is_empty() {
        let ring = &polys[0].exterior().0;
        if ring.len() == 4 {
            return Some(triangle_in_centre(ring[0], ring[1], ring[2]));
        }
        if ring.len() == 5
            && is_convex(ring)
            && let Some(c) = convex_quadrilateral_center(ring)
        {
            return Some(c);
        }
    }
    let segments: Vec<RLine<[f64; 2]>> = polys
        .iter()
        .flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors()))
        .flat_map(|r| r.0.windows(2).map(|w| RLine::new([w[0].x, w[0].y], [w[1].x, w[1].y])))
        .collect();
    let input = Polygonal { polys, boundary: RTree::bulk_load(segments) };
    let env = Envelope::of_geometry(polygonal);

    let mut queue = BinaryHeap::new();
    let cell_size = env.width().max(env.height());
    if cell_size > 0.0 {
        let c = env.centre().unwrap();
        let h_side = cell_size / 2.0;
        queue.push(Cell::new(c.x, c.y, h_side, input.distance_to_boundary(c.x, c.y)));
    }
    // initial candidate center point
    let ip = interior_point::interior_point(polygonal)?;
    let mut farthest = Cell::new(ip.x, ip.y, env.diameter(), input.distance_to_boundary(ip.x, ip.y));

    let max_iter = compute_maximum_iterations(&env, tolerance);
    let mut iter = 0;
    while let Some(cell) = queue.pop() {
        if iter >= max_iter {
            break;
        }
        iter += 1;
        if cell.distance > farthest.distance {
            farthest = Cell::new(cell.x, cell.y, cell.h_side, cell.distance);
        }
        let required_tol = if tolerance > 0.0 { tolerance } else { farthest.distance * AUTO_TOLERANCE_FRACTION };
        let potential_increase = cell.max_dist - farthest.distance;
        if potential_increase < required_tol {
            break;
        }
        let h2 = cell.h_side / 2.0;
        for (dx, dy) in [(-h2, -h2), (h2, -h2), (-h2, h2), (h2, h2)] {
            let (x, y) = (cell.x + dx, cell.y + dy);
            queue.push(Cell::new(x, y, h2, input.distance_to_boundary(x, y)));
        }
    }
    Some(Coord { x: farthest.x, y: farthest.y })
}

fn compute_maximum_iterations(env: &Envelope, tolerance_dist: f64) -> u64 {
    let diam = env.diameter();
    let tol_dist = if tolerance_dist <= 0.0 { 0.5 * diam * AUTO_TOLERANCE_FRACTION } else { tolerance_dist };
    let ncells = diam / tol_dist;
    let factor = (ncells.ln() as i64).max(1);
    (2000 + 2000 * factor) as u64
}

fn orient(p: Coord<f64>, q: Coord<f64>, r: Coord<f64>) -> i8 {
    match RobustKernel::orient2d(p, q, r) {
        Orientation::CounterClockwise => 1,
        Orientation::Clockwise => -1,
        Orientation::Collinear => 0,
    }
}

/// JTS `Triangle.inCentre`.
fn triangle_in_centre(a: Coord<f64>, b: Coord<f64>, c: Coord<f64>) -> Coord<f64> {
    let len0 = (b.x - c.x).hypot(b.y - c.y);
    let len1 = (a.x - c.x).hypot(a.y - c.y);
    let len2 = (a.x - b.x).hypot(a.y - b.y);
    let circum = len0 + len1 + len2;
    Coord { x: (len0 * a.x + len1 * b.x + len2 * c.x) / circum, y: (len0 * a.y + len1 * b.y + len2 * c.y) / circum }
}

fn is_convex(ring: &[Coord<f64>]) -> bool {
    let n = ring.len();
    if n < 4 {
        return false;
    }
    if n == 4 {
        return true;
    }
    let mut ring_orient = 0;
    for i in 0..n - 1 {
        let i1 = i + 1;
        let i2 = if i1 >= n - 1 { 1 } else { i1 + 1 };
        let o = orient(ring[i], ring[i1], ring[i2]);
        if o == 0 {
            continue;
        }
        if ring_orient == 0 {
            ring_orient = o;
        } else if o != ring_orient {
            return false;
        }
    }
    true
}

fn angle(p0: Coord<f64>, p1: Coord<f64>) -> f64 {
    (p1.y - p0.y).atan2(p1.x - p0.x)
}

fn normalize_angle(mut a: f64) -> f64 {
    use std::f64::consts::PI;
    while a > PI {
        a -= 2.0 * PI;
    }
    while a <= -PI {
        a += 2.0 * PI;
    }
    a
}

fn bisector(tip1: Coord<f64>, tail: Coord<f64>, tip2: Coord<f64>) -> f64 {
    use std::f64::consts::PI;
    let a1 = angle(tail, tip1);
    let a2 = angle(tail, tip2);
    let mut ang_del = a2 - a1;
    if ang_del <= -PI {
        ang_del += 2.0 * PI;
    } else if ang_del > PI {
        ang_del -= 2.0 * PI;
    }
    normalize_angle(a1 + ang_del / 2.0)
}

fn snap(v: f64) -> f64 {
    if v.abs() < 5e-16 { 0.0 } else { v }
}

fn closest_point(p0: Coord<f64>, p1: Coord<f64>, p: Coord<f64>) -> Coord<f64> {
    let factor = if p == p0 {
        0.0
    } else if p == p1 {
        1.0
    } else {
        let dx = p1.x - p0.x;
        let dy = p1.y - p0.y;
        let len_sq = dx * dx + dy * dy;
        if len_sq <= 0.0 { f64::NAN } else { ((p.x - p0.x) * dx + (p.y - p0.y) * dy) / len_sq }
    };
    if factor > 0.0 && factor < 1.0 {
        return Coord { x: p0.x + factor * (p1.x - p0.x), y: p0.y + factor * (p1.y - p0.y) };
    }
    let d0 = (p0.x - p.x).hypot(p0.y - p.y);
    let d1 = (p1.x - p.x).hypot(p1.y - p.y);
    if d0 < d1 { p0 } else { p1 }
}

/// JTS `ExactMaxInscribedCircle.computeConvexQuadrilateral`.
fn convex_quadrilateral_center(ring: &[Coord<f64>]) -> Option<Coord<f64>> {
    // orient clockwise
    let signed: f64 = ring.windows(2).map(|w| w[0].x * w[1].y - w[1].x * w[0].y).sum();
    let ring_cw: Vec<Coord<f64>> = if signed > 0.0 { ring.iter().rev().copied().collect() } else { ring.to_vec() };
    let mut env = Envelope::null();
    for c in &ring_cw {
        env.expand_to_include(c.x, c.y);
    }
    let diam = 2.0 * env.diameter();
    let bisectors: Vec<Line<f64>> = (0..4)
        .map(|i| {
            let base = ring_cw[i];
            let prev = ring_cw[if i == 0 { ring_cw.len() - 2 } else { i - 1 }];
            let next = ring_cw[i + 1];
            let ang = bisector(prev, base, next);
            Line::new(base, Coord { x: base.x + diam * snap(ang.cos()), y: base.y + diam * snap(ang.sin()) })
        })
        .collect();
    let mut max_dist = -1.0;
    let mut center = None;
    for i in 0..4 {
        let node = match line_intersection(bisectors[i], bisectors[(i + 1) % 4]) {
            Some(LineIntersection::SinglePoint { intersection, .. }) => intersection,
            Some(LineIntersection::Collinear { intersection }) => intersection.start,
            None => continue,
        };
        if ring_cw.windows(2).any(|w| orient(w[0], w[1], node) == 1) {
            continue;
        }
        let dist = ring_cw
            .windows(2)
            .map(|w| {
                let r = closest_point(w[0], w[1], node);
                (node.x - r.x).hypot(node.y - r.y)
            })
            .fold(f64::INFINITY, f64::min);
        if max_dist < 0.0 || dist > max_dist {
            max_dist = dist;
            center = Some(node);
        }
    }
    center
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::test_utils::*;

    #[test]
    fn square_center() {
        let c = get_center(&rectangle(0.0, 10.0).into(), 0.01).unwrap();
        assert!((c.x - 5.0).abs() < 1e-9 && (c.y - 5.0).abs() < 1e-9, "{c:?}");
    }

    #[test]
    fn triangle_center() {
        let c = get_center(&new_polygon(&[0.0, 0.0, 4.0, 0.0, 0.0, 3.0, 0.0, 0.0]).into(), 0.01).unwrap();
        // inradius of 3-4-5 triangle is 1
        assert!((c.x - 1.0).abs() < 1e-9 && (c.y - 1.0).abs() < 1e-9, "{c:?}");
    }

    #[test]
    fn u_shape_center_inside() {
        let u: Geometry<f64> = new_polygon(&[
            0.0, 0.0, 30.0, 0.0, 30.0, 30.0, 20.0, 30.0, 20.0, 10.0, 10.0, 10.0, 10.0, 30.0, 0.0, 30.0, 0.0, 0.0,
        ])
        .into();
        let c = get_center(&u, 0.1).unwrap();
        assert_point_on_surface(&u, &Geometry::Point(c.into()));
    }

    #[test]
    fn hole_avoided() {
        let p: Geometry<f64> =
            new_polygon_with_holes(rectangle_coord_list(0.0, 10.0), vec![rectangle_coord_list(4.0, 6.0)]).into();
        let c = get_center(&p, 0.01).unwrap();
        assert_point_on_surface(&p, &Geometry::Point(c.into()));
        assert!(!rectangle(4.0, 6.0).exterior().0.is_empty());
        assert!(!(c.x > 4.0 && c.x < 6.0 && c.y > 4.0 && c.y < 6.0));
    }
}

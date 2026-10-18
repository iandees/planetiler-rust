//! Ports of JTS `Centroid` and `InteriorPoint` (`InteriorPointArea`, `InteriorPointLine`, `InteriorPointPoint`) so
//! that label points match Java planetiler exactly.

use geo::Winding;
use geo_types::{Coord, Geometry, LineString, Polygon};

#[derive(Default)]
struct CentroidState {
    area_base_pt: Option<Coord<f64>>,
    areasum2: f64,
    cg3: Coord<f64>,
    line_cent_sum: Coord<f64>,
    total_length: f64,
    pt_count: usize,
    pt_cent_sum: Coord<f64>,
}

impl CentroidState {
    fn add(&mut self, geom: &Geometry<f64>) {
        match geom {
            Geometry::Point(p) => self.add_point(p.0),
            Geometry::Line(l) => self.add_line_segments(&[l.start, l.end]),
            Geometry::LineString(ls) => {
                if !ls.0.is_empty() {
                    self.add_line_segments(&ls.0)
                }
            }
            Geometry::Polygon(p) => self.add_polygon(p),
            Geometry::Rect(r) => self.add_polygon(&r.to_polygon()),
            Geometry::Triangle(t) => self.add_polygon(&t.to_polygon()),
            Geometry::MultiPoint(mp) => mp.0.iter().for_each(|p| self.add_point(p.0)),
            Geometry::MultiLineString(ml) => {
                for l in &ml.0 {
                    if !l.0.is_empty() {
                        self.add_line_segments(&l.0);
                    }
                }
            }
            Geometry::MultiPolygon(mp) => mp.0.iter().for_each(|p| self.add_polygon(p)),
            Geometry::GeometryCollection(gc) => gc.0.iter().for_each(|g| self.add(g)),
        }
    }

    fn add_polygon(&mut self, poly: &Polygon<f64>) {
        if poly.exterior().0.is_empty() {
            return;
        }
        self.add_shell(poly.exterior());
        for hole in poly.interiors() {
            self.add_hole(hole);
        }
    }

    fn add_shell(&mut self, ring: &LineString<f64>) {
        let pts = &ring.0;
        if let Some(first) = pts.first() {
            self.area_base_pt = Some(*first);
        }
        let positive = !ring.is_ccw();
        for w in pts.windows(2) {
            self.add_triangle(w[0], w[1], positive);
        }
        self.add_line_segments(pts);
    }

    fn add_hole(&mut self, ring: &LineString<f64>) {
        let pts = &ring.0;
        let positive = ring.is_ccw();
        for w in pts.windows(2) {
            self.add_triangle(w[0], w[1], positive);
        }
        self.add_line_segments(pts);
    }

    fn add_triangle(&mut self, p1: Coord<f64>, p2: Coord<f64>, positive: bool) {
        let p0 = self.area_base_pt.unwrap_or(p1);
        let sign = if positive { 1.0 } else { -1.0 };
        let cx = p0.x + p1.x + p2.x;
        let cy = p0.y + p1.y + p2.y;
        let area2 = (p1.x - p0.x) * (p2.y - p0.y) - (p2.x - p0.x) * (p1.y - p0.y);
        self.cg3.x += sign * area2 * cx;
        self.cg3.y += sign * area2 * cy;
        self.areasum2 += sign * area2;
    }

    fn add_line_segments(&mut self, pts: &[Coord<f64>]) {
        let mut line_len = 0.0;
        for w in pts.windows(2) {
            let segment_len = (w[0].x - w[1].x).hypot(w[0].y - w[1].y);
            if segment_len == 0.0 {
                continue;
            }
            line_len += segment_len;
            self.line_cent_sum.x += segment_len * (w[0].x + w[1].x) / 2.0;
            self.line_cent_sum.y += segment_len * (w[0].y + w[1].y) / 2.0;
        }
        self.total_length += line_len;
        if line_len == 0.0 && !pts.is_empty() {
            self.add_point(pts[0]);
        }
    }

    fn add_point(&mut self, pt: Coord<f64>) {
        self.pt_count += 1;
        self.pt_cent_sum.x += pt.x;
        self.pt_cent_sum.y += pt.y;
    }

    fn centroid(&self) -> Option<Coord<f64>> {
        if self.areasum2.abs() > 0.0 {
            Some(Coord { x: self.cg3.x / 3.0 / self.areasum2, y: self.cg3.y / 3.0 / self.areasum2 })
        } else if self.total_length > 0.0 {
            Some(Coord { x: self.line_cent_sum.x / self.total_length, y: self.line_cent_sum.y / self.total_length })
        } else if self.pt_count > 0 {
            Some(Coord { x: self.pt_cent_sum.x / self.pt_count as f64, y: self.pt_cent_sum.y / self.pt_count as f64 })
        } else {
            None
        }
    }
}

/// JTS `Centroid.getCentroid`: area-weighted centroid of polygons, else length-weighted centroid of lines, else
/// average of points. Returns `None` for empty geometries.
pub fn centroid(geom: &Geometry<f64>) -> Option<Coord<f64>> {
    let mut state = CentroidState::default();
    state.add(geom);
    state.centroid()
}

/// Dimension of the highest-dimension non-empty component, or -1 if empty.
fn dimension_non_empty(geom: &Geometry<f64>) -> i32 {
    match geom {
        Geometry::Point(_) => 0,
        Geometry::MultiPoint(mp) => {
            if mp.0.is_empty() {
                -1
            } else {
                0
            }
        }
        Geometry::Line(_) => 1,
        Geometry::LineString(ls) => {
            if ls.0.is_empty() {
                -1
            } else {
                1
            }
        }
        Geometry::MultiLineString(ml) => {
            if ml.0.iter().all(|l| l.0.is_empty()) {
                -1
            } else {
                1
            }
        }
        Geometry::Polygon(p) => {
            if p.exterior().0.is_empty() {
                -1
            } else {
                2
            }
        }
        Geometry::Rect(_) | Geometry::Triangle(_) => 2,
        Geometry::MultiPolygon(mp) => {
            if mp.0.iter().all(|p| p.exterior().0.is_empty()) {
                -1
            } else {
                2
            }
        }
        Geometry::GeometryCollection(gc) => gc.0.iter().map(dimension_non_empty).max().unwrap_or(-1),
    }
}

/// JTS `InteriorPoint.getInteriorPoint`: a point guaranteed to be in the interior of polygons (or on lines /
/// one of the points for lower-dimension geometries). Returns `None` for empty geometries.
pub fn interior_point(geom: &Geometry<f64>) -> Option<Coord<f64>> {
    match dimension_non_empty(geom) {
        0 => interior_point_point(geom),
        1 => interior_point_line(geom),
        2 => interior_point_area(geom),
        _ => None,
    }
}

struct Closest {
    centroid: Coord<f64>,
    min_distance: f64,
    point: Option<Coord<f64>>,
}

impl Closest {
    fn add(&mut self, p: Coord<f64>) {
        let dist = (p.x - self.centroid.x).hypot(p.y - self.centroid.y);
        if dist < self.min_distance {
            self.point = Some(p);
            self.min_distance = dist;
        }
    }
}

fn for_each_point(geom: &Geometry<f64>, f: &mut impl FnMut(Coord<f64>)) {
    match geom {
        Geometry::Point(p) => f(p.0),
        Geometry::MultiPoint(mp) => mp.0.iter().for_each(|p| f(p.0)),
        Geometry::GeometryCollection(gc) => gc.0.iter().for_each(|g| for_each_point(g, f)),
        _ => {}
    }
}

fn for_each_line(geom: &Geometry<f64>, f: &mut impl FnMut(&[Coord<f64>])) {
    match geom {
        Geometry::Line(l) => f(&[l.start, l.end]),
        Geometry::LineString(ls) if !ls.0.is_empty() => f(&ls.0),
        Geometry::MultiLineString(ml) => ml.0.iter().filter(|l| !l.0.is_empty()).for_each(|l| f(&l.0)),
        Geometry::GeometryCollection(gc) => gc.0.iter().for_each(|g| for_each_line(g, f)),
        _ => {}
    }
}

fn interior_point_point(geom: &Geometry<f64>) -> Option<Coord<f64>> {
    let mut closest = Closest { centroid: centroid(geom)?, min_distance: f64::MAX, point: None };
    for_each_point(geom, &mut |p| closest.add(p));
    closest.point
}

fn interior_point_line(geom: &Geometry<f64>) -> Option<Coord<f64>> {
    let mut closest = Closest { centroid: centroid(geom)?, min_distance: f64::MAX, point: None };
    for_each_line(geom, &mut |pts| {
        for p in pts.iter().take(pts.len().saturating_sub(1)).skip(1) {
            closest.add(*p);
        }
    });
    if closest.point.is_none() {
        for_each_line(geom, &mut |pts| {
            closest.add(pts[0]);
            closest.add(pts[pts.len() - 1]);
        });
    }
    closest.point
}

fn for_each_polygon(geom: &Geometry<f64>, f: &mut impl FnMut(&Polygon<f64>)) {
    match geom {
        Geometry::Polygon(p) => f(p),
        Geometry::Rect(r) => f(&r.to_polygon()),
        Geometry::Triangle(t) => f(&t.to_polygon()),
        Geometry::MultiPolygon(mp) => mp.0.iter().for_each(|p| f(p)),
        Geometry::GeometryCollection(gc) => gc.0.iter().for_each(|g| for_each_polygon(g, f)),
        _ => {}
    }
}

fn interior_point_area(geom: &Geometry<f64>) -> Option<Coord<f64>> {
    let mut max_width = -1.0;
    let mut result = None;
    for_each_polygon(geom, &mut |poly| {
        if poly.exterior().0.is_empty() {
            return;
        }
        let (pt, width) = interior_point_polygon(poly);
        if width > max_width {
            max_width = width;
            result = Some(pt);
        }
    });
    result
}

fn scan_line_y(poly: &Polygon<f64>) -> f64 {
    let (mut lo_y, mut hi_y) = (f64::INFINITY, f64::NEG_INFINITY);
    for c in poly.exterior().0.iter() {
        lo_y = lo_y.min(c.y);
        hi_y = hi_y.max(c.y);
    }
    let centre_y = (lo_y + hi_y) / 2.0;
    for ring in std::iter::once(poly.exterior()).chain(poly.interiors()) {
        for c in &ring.0 {
            let y = c.y;
            if y <= centre_y {
                if y > lo_y {
                    lo_y = y;
                }
            } else if y < hi_y {
                hi_y = y;
            }
        }
    }
    (hi_y + lo_y) / 2.0
}

/// Returns the interior point of one polygon and the width of the interior section it is at.
fn interior_point_polygon(poly: &Polygon<f64>) -> (Coord<f64>, f64) {
    let scan_y = scan_line_y(poly);
    let mut crossings: Vec<f64> = Vec::new();
    for ring in std::iter::once(poly.exterior()).chain(poly.interiors()) {
        let (min_y, max_y) =
            ring.0.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), c| (lo.min(c.y), hi.max(c.y)));
        if scan_y < min_y || scan_y > max_y {
            continue;
        }
        for w in ring.0.windows(2) {
            let (p0, p1) = (w[0], w[1]);
            if (p0.y > scan_y && p1.y > scan_y) || (p0.y < scan_y && p1.y < scan_y) {
                continue;
            }
            // edge crossing counted?
            if p0.y == p1.y || (p0.y == scan_y && p1.y < scan_y) || (p1.y == scan_y && p0.y < scan_y) {
                continue;
            }
            let x = if p0.x == p1.x {
                p0.x
            } else {
                let m = (p1.y - p0.y) / (p1.x - p0.x);
                p0.x + ((scan_y - p0.y) / m)
            };
            crossings.push(x);
        }
    }
    let mut interior_point = poly.exterior().0[0];
    let mut width = 0.0;
    if crossings.is_empty() {
        return (interior_point, width);
    }
    debug_assert!(
        crossings.len().is_multiple_of(2),
        "Interior Point robustness failure: odd number of scanline crossings"
    );
    crossings.sort_by(f64::total_cmp);
    for pair in crossings.chunks_exact(2) {
        let (x1, x2) = (pair[0], pair[1]);
        if x2 - x1 > width {
            width = x2 - x1;
            interior_point = Coord { x: (x1 + x2) / 2.0, y: scan_y };
        }
    }
    (interior_point, width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::test_utils::*;

    #[test]
    fn centroid_polygon() {
        assert_eq!(Some(Coord { x: 0.5, y: 0.5 }), centroid(&rectangle(0.0, 1.0).into()));
        let with_hole: Geometry<f64> =
            new_polygon_with_holes(rectangle_coord_list(0.0, 4.0), vec![rectangle_coord_list(0.0, 2.0)]).into();
        let c = centroid(&with_hole).unwrap();
        // area 16 at (2,2) minus area 4 at (1,1)
        assert!((c.x - (32.0 - 4.0) / 12.0).abs() < 1e-12 && (c.y - (32.0 - 4.0) / 12.0).abs() < 1e-12);
    }

    #[test]
    fn centroid_line_and_points() {
        assert_eq!(Some(Coord { x: 1.0, y: 0.0 }), centroid(&new_line_string(&[0.0, 0.0, 2.0, 0.0]).into()));
        assert_eq!(
            Some(Coord { x: 1.0, y: 1.0 }),
            centroid(&new_multi_point(vec![new_point(0.0, 0.0), new_point(2.0, 2.0)]).into())
        );
        assert_eq!(None, centroid(&empty_geometry()));
    }

    #[test]
    fn interior_point_area_test() {
        // U shape: centroid is outside, interior point must be inside
        let u: Geometry<f64> =
            new_polygon(&[0.0, 0.0, 3.0, 0.0, 3.0, 3.0, 2.0, 3.0, 2.0, 1.0, 1.0, 1.0, 1.0, 3.0, 0.0, 3.0, 0.0, 0.0])
                .into();
        let p = interior_point(&u).unwrap();
        assert_point_on_surface(&u, &Geometry::Point(p.into()));
        assert_eq!(Coord { x: 0.5, y: 2.0 }, p);
    }

    #[test]
    fn interior_point_line_test() {
        assert_eq!(
            Some(Coord { x: 1.0, y: 1.0 }),
            interior_point(&new_line_string(&[0.0, 0.0, 1.0, 1.0, 2.0, 0.0]).into())
        );
        assert_eq!(Some(Coord { x: 0.0, y: 0.0 }), interior_point(&new_line_string(&[0.0, 0.0, 2.0, 0.0]).into()));
    }
}

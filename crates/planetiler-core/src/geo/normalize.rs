//! Port of JTS `Geometry.normalize()` / `norm()` and `Geometry.compareTo()`: puts a geometry into a canonical form so
//! that geometries with the same structure compare equal regardless of ring start point, orientation or element order.

use std::cmp::Ordering;

use geo::Winding;
use geo_types::{Coord, Geometry, GeometryCollection, LineString, MultiLineString, MultiPoint, MultiPolygon, Polygon};

fn cmp_coord(a: &Coord<f64>, b: &Coord<f64>) -> Ordering {
    a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y))
}

fn cmp_coords(a: &[Coord<f64>], b: &[Coord<f64>]) -> Ordering {
    for (ca, cb) in a.iter().zip(b) {
        let c = cmp_coord(ca, cb);
        if c != Ordering::Equal {
            return c;
        }
    }
    a.len().cmp(&b.len())
}

fn type_code(g: &Geometry<f64>) -> u8 {
    match g {
        Geometry::Point(_) => 0,
        Geometry::MultiPoint(_) => 1,
        Geometry::Line(_) | Geometry::LineString(_) => 2,
        Geometry::MultiLineString(_) => 4,
        Geometry::Polygon(_) | Geometry::Rect(_) | Geometry::Triangle(_) => 5,
        Geometry::MultiPolygon(_) => 6,
        Geometry::GeometryCollection(_) => 7,
    }
}

fn cmp_polygon(a: &Polygon<f64>, b: &Polygon<f64>) -> Ordering {
    cmp_coords(&a.exterior().0, &b.exterior().0).then_with(|| {
        for (ha, hb) in a.interiors().iter().zip(b.interiors()) {
            let c = cmp_coords(&ha.0, &hb.0);
            if c != Ordering::Equal {
                return c;
            }
        }
        a.interiors().len().cmp(&b.interiors().len())
    })
}

fn cmp_slices<T>(a: &[T], b: &[T], f: impl Fn(&T, &T) -> Ordering) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        let c = f(x, y);
        if c != Ordering::Equal {
            return c;
        }
    }
    a.len().cmp(&b.len())
}

/// JTS `Geometry.compareTo`.
pub fn compare_geometries(a: &Geometry<f64>, b: &Geometry<f64>) -> Ordering {
    let (ta, tb) = (type_code(a), type_code(b));
    if ta != tb {
        return ta.cmp(&tb);
    }
    match (a, b) {
        (Geometry::Point(a), Geometry::Point(b)) => cmp_coord(&a.0, &b.0),
        (Geometry::MultiPoint(a), Geometry::MultiPoint(b)) => cmp_slices(&a.0, &b.0, |x, y| cmp_coord(&x.0, &y.0)),
        (Geometry::LineString(a), Geometry::LineString(b)) => cmp_coords(&a.0, &b.0),
        (Geometry::MultiLineString(a), Geometry::MultiLineString(b)) => {
            cmp_slices(&a.0, &b.0, |x, y| cmp_coords(&x.0, &y.0))
        }
        (Geometry::Polygon(a), Geometry::Polygon(b)) => cmp_polygon(a, b),
        (Geometry::MultiPolygon(a), Geometry::MultiPolygon(b)) => cmp_slices(&a.0, &b.0, cmp_polygon),
        (Geometry::GeometryCollection(a), Geometry::GeometryCollection(b)) => {
            cmp_slices(&a.0, &b.0, compare_geometries)
        }
        _ => compare_geometries(&to_basic(a), &to_basic(b)),
    }
}

fn to_basic(g: &Geometry<f64>) -> Geometry<f64> {
    match g {
        Geometry::Line(l) => Geometry::LineString((*l).into()),
        Geometry::Rect(r) => Geometry::Polygon(r.to_polygon()),
        Geometry::Triangle(t) => Geometry::Polygon(t.to_polygon()),
        other => other.clone(),
    }
}

fn normalize_line(ls: &mut LineString<f64>) {
    let n = ls.0.len();
    for i in 0..n / 2 {
        let j = n - 1 - i;
        if ls.0[i] != ls.0[j] {
            if cmp_coord(&ls.0[i], &ls.0[j]) == Ordering::Greater {
                ls.0.reverse();
            }
            return;
        }
    }
}

fn normalize_ring(ring: &mut LineString<f64>, clockwise: bool) {
    if ring.0.len() < 2 {
        return;
    }
    let unique = &ring.0[..ring.0.len() - 1];
    let min_idx = unique.iter().enumerate().min_by(|a, b| cmp_coord(a.1, b.1)).map(|(i, _)| i).unwrap_or(0);
    let mut coords: Vec<Coord<f64>> = unique[min_idx..].iter().chain(&unique[..min_idx]).copied().collect();
    coords.push(coords[0]);
    *ring = LineString(coords);
    if ring.is_ccw() == clockwise {
        ring.0.reverse();
    }
}

fn normalize_polygon(p: &mut Polygon<f64>) {
    p.exterior_mut(|e| normalize_ring(e, true));
    p.interiors_mut(|holes| {
        for h in holes.iter_mut() {
            normalize_ring(h, false);
        }
        holes.sort_by(|a, b| cmp_coords(&a.0, &b.0));
    });
}

/// Normalize `geom` in place (JTS `Geometry.normalize()`): polygon shells clockwise, holes counter-clockwise, rings
/// starting at their minimum coordinate, and collection elements sorted.
pub fn normalize(geom: &mut Geometry<f64>) {
    match geom {
        Geometry::Point(_) => {}
        Geometry::Line(_) | Geometry::Rect(_) | Geometry::Triangle(_) => {
            *geom = to_basic(geom);
            normalize(geom);
        }
        Geometry::LineString(ls) => normalize_line(ls),
        Geometry::Polygon(p) => normalize_polygon(p),
        Geometry::MultiPoint(MultiPoint(points)) => points.sort_by(|a, b| cmp_coord(&a.0, &b.0)),
        Geometry::MultiLineString(MultiLineString(lines)) => {
            lines.iter_mut().for_each(normalize_line);
            lines.sort_by(|a, b| cmp_coords(&a.0, &b.0));
        }
        Geometry::MultiPolygon(MultiPolygon(polys)) => {
            polys.iter_mut().for_each(normalize_polygon);
            polys.sort_by(cmp_polygon);
        }
        Geometry::GeometryCollection(GeometryCollection(geoms)) => {
            geoms.iter_mut().for_each(normalize);
            geoms.sort_by(compare_geometries);
        }
    }
}

/// Returns a normalized copy of `geom` (JTS `Geometry.norm()`).
pub fn norm(geom: &Geometry<f64>) -> Geometry<f64> {
    let mut copy = geom.clone();
    normalize(&mut copy);
    copy
}

/// JTS `Geometry.equalsNorm`: true if the normalized forms of both geometries are exactly equal.
pub fn equals_norm(a: &Geometry<f64>, b: &Geometry<f64>) -> bool {
    norm(a) == norm(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::test_utils::*;

    #[test]
    fn normalize_polygon_rotation_and_orientation() {
        let a: Geometry<f64> = new_polygon(&[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 0.0]).into();
        let b: Geometry<f64> = new_polygon(&[1.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0]).into();
        assert!(equals_norm(&a, &b));
        assert_eq!(Geometry::Polygon(new_polygon(&[0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0])), norm(&a));
    }

    #[test]
    fn normalize_line() {
        let a: Geometry<f64> = new_line_string(&[1.0, 1.0, 0.0, 0.0]).into();
        assert_eq!(Geometry::LineString(new_line_string(&[0.0, 0.0, 1.0, 1.0])), norm(&a));
    }
}

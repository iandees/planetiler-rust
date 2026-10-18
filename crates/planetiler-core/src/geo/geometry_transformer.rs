//! Port of JTS `GeometryTransformer`: rebuilds a geometry after transforming each coordinate sequence, following the
//! same rules JTS uses for degenerate results (rings with fewer than 4 points become line strings, empty components
//! are pruned from collections, etc).

use geo_types::{
    Coord, Geometry, GeometryCollection, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon,
};

/// Transforms the coordinate sequences of a geometry. Implementors only need to provide
/// [`GeometryTransformer::transform_coordinates`]; [`GeometryTransformer::transform`] rebuilds the geometry.
pub trait GeometryTransformer {
    /// Transform one coordinate sequence. `is_ring` is true when the sequence is a polygon ring (JTS `LinearRing`
    /// parent). Return `None` for JTS `null` (the component becomes empty).
    fn transform_coordinates(&mut self, coords: &[Coord<f64>], is_ring: bool) -> Option<Vec<Coord<f64>>>;

    /// When true, polygon rings that degenerate into a line (fewer than 4 points) are removed (holes) or make the
    /// whole polygon empty (shells). This is the behavior of planetiler's simplifiers.
    fn remove_degenerate_rings(&self) -> bool {
        false
    }

    /// Transform `geom`. Returns an empty geometry collection when the result is JTS `null`.
    fn transform(&mut self, geom: &Geometry<f64>) -> Geometry<f64>
    where
        Self: Sized,
    {
        transform_geometry(self, geom).unwrap_or_else(empty_geometry)
    }
}

/// An empty geometry collection, equivalent to JTS `GeometryFactory.createGeometryCollection()`.
pub fn empty_geometry() -> Geometry<f64> {
    Geometry::GeometryCollection(GeometryCollection(vec![]))
}

/// An empty polygon.
pub fn empty_polygon() -> Polygon<f64> {
    Polygon::new(LineString(vec![]), vec![])
}

/// Returns true if `geom` has no coordinates (JTS `isEmpty`).
pub fn is_empty(geom: &Geometry<f64>) -> bool {
    match geom {
        Geometry::Point(_) | Geometry::Line(_) | Geometry::Rect(_) | Geometry::Triangle(_) => false,
        Geometry::LineString(ls) => ls.0.is_empty(),
        Geometry::Polygon(p) => p.exterior().0.is_empty(),
        Geometry::MultiPoint(mp) => mp.0.is_empty(),
        Geometry::MultiLineString(mls) => mls.0.iter().all(|l| l.0.is_empty()),
        Geometry::MultiPolygon(mp) => mp.0.iter().all(|p| p.exterior().0.is_empty()),
        Geometry::GeometryCollection(gc) => gc.0.iter().all(is_empty),
    }
}

/// Component class used by [`build_geometry`] to mirror JTS `GeometryFactory.buildGeometry` semantics.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Point,
    LineString,
    LinearRing,
    Polygon,
    Collection,
}

fn kind_of(geom: &Geometry<f64>) -> Kind {
    match geom {
        Geometry::Point(_) => Kind::Point,
        Geometry::Line(_) | Geometry::LineString(_) => Kind::LineString,
        Geometry::Polygon(_) | Geometry::Rect(_) | Geometry::Triangle(_) => Kind::Polygon,
        _ => Kind::Collection,
    }
}

/// JTS `GeometryFactory.buildGeometry`: the "smallest" geometry that can hold all of `geoms`.
pub fn build_geometry(geoms: Vec<Geometry<f64>>) -> Geometry<f64> {
    let kinds: Vec<Kind> = geoms.iter().map(kind_of).collect();
    build_geometry_with_kinds(geoms, &kinds)
}

fn build_geometry_with_kinds(geoms: Vec<Geometry<f64>>, kinds: &[Kind]) -> Geometry<f64> {
    if geoms.is_empty() {
        return empty_geometry();
    }
    let first = kinds[0];
    let heterogeneous = kinds.iter().any(|k| *k != first);
    if heterogeneous || first == Kind::Collection {
        return Geometry::GeometryCollection(GeometryCollection(geoms));
    }
    if geoms.len() == 1 {
        return geoms.into_iter().next().unwrap();
    }
    match first {
        Kind::Point => Geometry::MultiPoint(MultiPoint(
            geoms.into_iter().filter_map(|g| if let Geometry::Point(p) = g { Some(p) } else { None }).collect(),
        )),
        Kind::LineString | Kind::LinearRing => Geometry::MultiLineString(MultiLineString(
            geoms
                .into_iter()
                .filter_map(|g| match g {
                    Geometry::LineString(l) => Some(l),
                    Geometry::Line(l) => Some(l.into()),
                    _ => None,
                })
                .collect(),
        )),
        Kind::Polygon => Geometry::MultiPolygon(MultiPolygon(
            geoms
                .into_iter()
                .filter_map(|g| match g {
                    Geometry::Polygon(p) => Some(p),
                    Geometry::Rect(r) => Some(r.to_polygon()),
                    Geometry::Triangle(t) => Some(t.to_polygon()),
                    _ => None,
                })
                .collect(),
        )),
        Kind::Collection => unreachable!(),
    }
}

enum RingResult {
    /// JTS `null` (only produced when [`GeometryTransformer::remove_degenerate_rings`] is set)
    Null,
    /// an empty linear ring
    Empty,
    /// sequence of 1-3 points (JTS downgrades it to a `LineString`)
    Line(Vec<Coord<f64>>),
    Ring(Vec<Coord<f64>>),
}

fn transform_ring<T: GeometryTransformer + ?Sized>(t: &mut T, ring: &LineString<f64>) -> RingResult {
    let result = match t.transform_coordinates(&ring.0, true) {
        None => RingResult::Empty,
        Some(seq) if seq.is_empty() => RingResult::Empty,
        Some(seq) if seq.len() < 4 => RingResult::Line(seq),
        Some(seq) => RingResult::Ring(seq),
    };
    // planetiler's simplifiers return null for rings that are no longer linear rings
    if t.remove_degenerate_rings() && matches!(result, RingResult::Line(_)) { RingResult::Null } else { result }
}

fn transform_polygon<T: GeometryTransformer + ?Sized>(t: &mut T, poly: &Polygon<f64>) -> Option<Geometry<f64>> {
    if poly.exterior().0.is_empty() {
        if t.remove_degenerate_rings() {
            return None;
        }
        return Some(Geometry::Polygon(empty_polygon()));
    }
    let shell = transform_ring(t, poly.exterior());
    let mut all_valid = matches!(shell, RingResult::Ring(_));
    let mut holes = Vec::with_capacity(poly.interiors().len());
    for hole in poly.interiors() {
        match transform_ring(t, hole) {
            RingResult::Empty | RingResult::Null => {}
            r @ RingResult::Line(_) => {
                all_valid = false;
                holes.push(r);
            }
            r => holes.push(r),
        }
    }
    if all_valid {
        let shell = match shell {
            RingResult::Ring(s) => LineString(s),
            _ => unreachable!(),
        };
        let holes = holes
            .into_iter()
            .map(|h| match h {
                RingResult::Ring(h) => LineString(h),
                _ => unreachable!(),
            })
            .collect();
        Some(Geometry::Polygon(Polygon::new(shell, holes)))
    } else {
        let mut geoms = Vec::new();
        let mut kinds = Vec::new();
        for ring in std::iter::once(shell).chain(holes) {
            match ring {
                RingResult::Ring(s) => {
                    geoms.push(Geometry::LineString(LineString(s)));
                    kinds.push(Kind::LinearRing);
                }
                RingResult::Empty => {
                    geoms.push(Geometry::LineString(LineString(vec![])));
                    kinds.push(Kind::LinearRing);
                }
                RingResult::Line(s) => {
                    geoms.push(Geometry::LineString(LineString(s)));
                    kinds.push(Kind::LineString);
                }
                RingResult::Null => {}
            }
        }
        Some(build_geometry_with_kinds(geoms, &kinds))
    }
}

fn push_non_empty(out: &mut Vec<Geometry<f64>>, geom: Option<Geometry<f64>>) {
    if let Some(g) = geom
        && !is_empty(&g)
    {
        out.push(g);
    }
}

/// Apply a [`GeometryTransformer`] to `geom`, returning `None` for JTS `null`.
pub fn transform_geometry<T: GeometryTransformer + ?Sized>(t: &mut T, geom: &Geometry<f64>) -> Option<Geometry<f64>> {
    match geom {
        Geometry::Point(p) => transform_point(t, p),
        Geometry::Line(l) => transform_geometry(t, &Geometry::LineString((*l).into())),
        Geometry::LineString(ls) => {
            let coords = t.transform_coordinates(&ls.0, false).unwrap_or_default();
            Some(Geometry::LineString(LineString(coords)))
        }
        Geometry::Polygon(p) => transform_polygon(t, p),
        Geometry::Rect(r) => transform_polygon(t, &r.to_polygon()),
        Geometry::Triangle(tr) => transform_polygon(t, &tr.to_polygon()),
        Geometry::MultiPoint(mp) => {
            let mut out = Vec::with_capacity(mp.0.len());
            for p in &mp.0 {
                push_non_empty(&mut out, transform_point(t, p));
            }
            if out.is_empty() {
                return Some(Geometry::MultiPoint(MultiPoint(vec![])));
            }
            Some(build_geometry(out))
        }
        Geometry::MultiLineString(mls) => {
            let mut out = Vec::with_capacity(mls.0.len());
            for l in &mls.0 {
                let coords = t.transform_coordinates(&l.0, false).unwrap_or_default();
                push_non_empty(&mut out, Some(Geometry::LineString(LineString(coords))));
            }
            if out.is_empty() {
                return Some(Geometry::MultiLineString(MultiLineString(vec![])));
            }
            Some(build_geometry(out))
        }
        Geometry::MultiPolygon(mp) => {
            let mut out = Vec::with_capacity(mp.0.len());
            for p in &mp.0 {
                push_non_empty(&mut out, transform_polygon(t, p));
            }
            if out.is_empty() {
                return Some(Geometry::MultiPolygon(MultiPolygon(vec![])));
            }
            Some(build_geometry(out))
        }
        Geometry::GeometryCollection(gc) => {
            let mut out = Vec::with_capacity(gc.0.len());
            for g in &gc.0 {
                push_non_empty(&mut out, transform_geometry(t, g));
            }
            Some(Geometry::GeometryCollection(GeometryCollection(out)))
        }
    }
}

fn transform_point<T: GeometryTransformer + ?Sized>(t: &mut T, p: &Point<f64>) -> Option<Geometry<f64>> {
    t.transform_coordinates(&[p.0], false).and_then(|c| c.first().copied()).map(|c| Geometry::Point(Point(c)))
}

/// A [`GeometryTransformer`] built from a closure.
pub struct FnTransformer<F: FnMut(&[Coord<f64>], bool) -> Option<Vec<Coord<f64>>>>(pub F);

impl<F: FnMut(&[Coord<f64>], bool) -> Option<Vec<Coord<f64>>>> GeometryTransformer for FnTransformer<F> {
    fn transform_coordinates(&mut self, coords: &[Coord<f64>], is_ring: bool) -> Option<Vec<Coord<f64>>> {
        (self.0)(coords, is_ring)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::test_utils::*;

    #[test]
    fn identity() {
        let mut t = FnTransformer(|c: &[Coord<f64>], _| Some(c.to_vec()));
        let poly: Geometry<f64> = rectangle(0.0, 1.0).into();
        assert_eq!(poly, t.transform(&poly));
    }

    #[test]
    fn degenerate_ring_becomes_line() {
        let mut t = FnTransformer(|c: &[Coord<f64>], _| Some(c[..3].to_vec()));
        let poly: Geometry<f64> = rectangle(0.0, 1.0).into();
        assert_eq!(Geometry::LineString(new_line_string(&[0.0, 0.0, 1.0, 0.0, 1.0, 1.0])), t.transform(&poly));
    }

    #[test]
    fn prune_empty_from_multi() {
        let mut t = FnTransformer(|c: &[Coord<f64>], _| if c[0].x > 1.0 { None } else { Some(c.to_vec()) });
        let mp: Geometry<f64> = new_multi_polygon(vec![rectangle(0.0, 1.0), rectangle(2.0, 3.0)]).into();
        assert_eq!(Geometry::Polygon(rectangle(0.0, 1.0)), t.transform(&mp));
    }
}

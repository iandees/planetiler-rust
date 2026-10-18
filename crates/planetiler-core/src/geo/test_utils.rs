//! Geometry construction and comparison helpers ported from planetiler's `TestUtils.java`.
//!
//! These are not `#[cfg(test)]`-gated so that integration tests and other crates can use them.

use geo::{MapCoords, Relate, Winding};
use geo_types::{
    Coord, Geometry, GeometryCollection, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon,
};

use crate::geo::geo_utils;
use crate::geo::normalize::norm;
use crate::geo::precision_model::java_round;

/// Coordinates from a flat list of `x, y, x, y, ...` values.
pub fn new_coordinate_list(coords: &[f64]) -> Vec<Coord<f64>> {
    assert!(coords.len().is_multiple_of(2), "odd number of ordinates");
    coords.as_chunks::<2>().0.iter().map(|c| Coord { x: c[0], y: c[1] }).collect()
}

/// A polygon with no holes from a flat list of `x, y, ...` ordinates for the outer ring.
pub fn new_polygon(coords: &[f64]) -> Polygon<f64> {
    Polygon::new(LineString(new_coordinate_list(coords)), vec![])
}

/// A polygon from coordinate lists for the outer ring and holes.
pub fn new_polygon_with_holes(outer: Vec<Coord<f64>>, inner: Vec<Vec<Coord<f64>>>) -> Polygon<f64> {
    Polygon::new(LineString(outer), inner.into_iter().map(LineString).collect())
}

pub fn new_polygon_from_coords(outer: Vec<Coord<f64>>) -> Polygon<f64> {
    new_polygon_with_holes(outer, vec![])
}

pub fn rectangle_coord_list_xy(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Vec<Coord<f64>> {
    new_coordinate_list(&[min_x, min_y, max_x, min_y, max_x, max_y, min_x, max_y, min_x, min_y])
}

pub fn rectangle_coord_list(min: f64, max: f64) -> Vec<Coord<f64>> {
    rectangle_coord_list_xy(min, min, max, max)
}

pub fn rectangle_xy(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Polygon<f64> {
    new_polygon_from_coords(rectangle_coord_list_xy(min_x, min_y, max_x, max_y))
}

pub fn rectangle(min: f64, max: f64) -> Polygon<f64> {
    rectangle_xy(min, min, max, max)
}

pub fn tile_bottom_right(buffer: f64) -> Polygon<f64> {
    rectangle_xy(128.0, 128.0, 256.0 + buffer, 256.0 + buffer)
}

pub fn tile_bottom(buffer: f64) -> Polygon<f64> {
    rectangle_xy(-buffer, 128.0, 256.0 + buffer, 256.0 + buffer)
}

pub fn tile_bottom_left(buffer: f64) -> Polygon<f64> {
    rectangle_xy(-buffer, 128.0, 128.0, 256.0 + buffer)
}

pub fn tile_left(buffer: f64) -> Polygon<f64> {
    rectangle_xy(-buffer, -buffer, 128.0, 256.0 + buffer)
}

pub fn tile_top_left(buffer: f64) -> Polygon<f64> {
    rectangle_xy(-buffer, -buffer, 128.0, 128.0)
}

pub fn tile_top(buffer: f64) -> Polygon<f64> {
    rectangle_xy(-buffer, -buffer, 256.0 + buffer, 128.0)
}

pub fn tile_top_right(buffer: f64) -> Polygon<f64> {
    rectangle_xy(128.0, -buffer, 256.0 + buffer, 128.0)
}

pub fn tile_right(buffer: f64) -> Polygon<f64> {
    rectangle_xy(128.0, -buffer, 256.0 + buffer, 256.0 + buffer)
}

pub fn tile_fill(buffer: f64) -> Vec<Coord<f64>> {
    rectangle_coord_list(-buffer, 256.0 + buffer)
}

pub fn new_line_string(coords: &[f64]) -> LineString<f64> {
    LineString(new_coordinate_list(coords))
}

pub fn new_line_string_from_coords(coords: Vec<Coord<f64>>) -> LineString<f64> {
    LineString(coords)
}

/// A closed ring (JTS `LinearRing`) from flat ordinates; the caller must close it.
pub fn new_linear_ring(coords: &[f64]) -> LineString<f64> {
    new_line_string(coords)
}

pub fn new_multi_line_string(lines: Vec<LineString<f64>>) -> MultiLineString<f64> {
    MultiLineString(lines)
}

pub fn new_point(x: f64, y: f64) -> Point<f64> {
    Point::new(x, y)
}

pub fn new_multi_point(points: Vec<Point<f64>>) -> MultiPoint<f64> {
    MultiPoint(points)
}

pub fn new_multi_polygon(polys: Vec<Polygon<f64>>) -> MultiPolygon<f64> {
    MultiPolygon(polys)
}

pub fn new_geometry_collection(geoms: Vec<Geometry<f64>>) -> GeometryCollection<f64> {
    GeometryCollection(geoms)
}

pub fn empty_geometry() -> Geometry<f64> {
    Geometry::GeometryCollection(GeometryCollection(vec![]))
}

/// Coordinates from world `x, y, ...` values converted to lon/lat.
pub fn world_coordinate_list(coords: &[f64]) -> Vec<Coord<f64>> {
    new_coordinate_list(coords)
        .into_iter()
        .map(|c| Coord { x: geo_utils::get_world_lon(c.x), y: geo_utils::get_world_lat(c.y) })
        .collect()
}

pub fn world_rectangle(min: f64, max: f64) -> Vec<Coord<f64>> {
    world_coordinate_list(&[min, min, max, min, max, max, min, max, min, min])
}

/// Round every ordinate to the nearest `1 / delta`.
pub fn round_to(input: &Geometry<f64>, delta: f64) -> Geometry<f64> {
    input.map_coords(|c| Coord { x: java_round(c.x * delta) / delta, y: java_round(c.y * delta) / delta })
}

/// Round every ordinate to 5 decimal places.
pub fn round(input: &Geometry<f64>) -> Geometry<f64> {
    round_to(input, 1e5)
}

/// Scale from 4096-unit vector tile coordinates to 256px tile coordinates (Java's `TRANSFORM_TO_TILE`).
pub fn transform_to_tile(input: &Geometry<f64>) -> Geometry<f64> {
    input.map_coords(|c| Coord { x: c.x * 256.0 / 4096.0, y: c.y * 256.0 / 4096.0 })
}

/// Assert that 2 geometries are equal after normalization (Java's `NormGeometry` comparison).
#[track_caller]
pub fn assert_same_normalized_feature(expected: &Geometry<f64>, actual: &Geometry<f64>) {
    let (e, a) = (norm(expected), norm(actual));
    assert_eq!(e, a, "expected (normalized) != actual (normalized)");
}

/// Assert that 2 geometries are equal after rounding to 5 decimal places and normalizing (Java's `RoundGeometry`).
#[track_caller]
pub fn assert_same_rounded_feature(expected: &Geometry<f64>, actual: &Geometry<f64>) {
    assert_eq!(norm(&round(expected)), norm(&round(actual)));
}

/// Assert that 2 geometries are topologically equal (Java's `TopoGeometry` / JTS `equalsTopo`).
#[track_caller]
pub fn assert_topologically_equivalent_feature(expected: &Geometry<f64>, actual: &Geometry<f64>) {
    assert!(expected.relate(actual).is_equal_topo(), "not topologically equal:\n{expected:?}\n{actual:?}");
}

/// Assert that `actual` is inside or on the boundary of `surface`.
#[track_caller]
pub fn assert_point_on_surface(surface: &Geometry<f64>, actual: &Geometry<f64>) {
    assert!(surface.relate(actual).is_covers(), "{actual:?}\nis not inside\n{surface:?}");
}

/// Java's `validateGeometry`: points non-empty, lines have at least 2 points, polygon shells are CCW and holes CW
/// (in planetiler's y-down tile coordinates JTS `isCCW` means what geo calls counter-clockwise), and polygons are
/// simple (JTS `isSimple()`: no ring self-intersects; rings of different polygons may overlap, e.g. after merging).
#[track_caller]
pub fn validate_geometry(g: &Geometry<f64>) {
    if matches!(g, Geometry::Polygon(_) | Geometry::MultiPolygon(_)) {
        assert!(is_simple_polygonal(g), "JTS isSimple(): {g:?}");
    }
    validate_geometry_recursive(g);
}

pub use crate::geo::is_valid::is_simple_polygonal;

#[track_caller]
fn validate_geometry_recursive(g: &Geometry<f64>) {
    match g {
        Geometry::GeometryCollection(gc) => gc.0.iter().for_each(validate_geometry),
        Geometry::MultiPolygon(mp) => mp.0.iter().for_each(|p| validate_geometry(&Geometry::Polygon(p.clone()))),
        Geometry::MultiLineString(ml) => ml.0.iter().for_each(|l| validate_geometry(&Geometry::LineString(l.clone()))),
        Geometry::MultiPoint(_) | Geometry::Point(_) => {}
        Geometry::LineString(line) => assert!(line.0.len() >= 2, "too few points: {line:?}"),
        Geometry::Polygon(poly) => {
            let outer = poly.exterior();
            assert!(outer.is_ccw(), "outer not CCW: {poly:?}");
            assert!(outer.0.len() >= 4, "outer too few points: {poly:?}");
            assert!(outer.is_closed(), "outer not closed: {poly:?}");
            for (i, inner) in poly.interiors().iter().enumerate() {
                assert!(!inner.is_ccw(), "inner {i} not CW: {poly:?}");
                assert!(inner.is_closed(), "inner {i} not closed: {poly:?}");
            }
        }
        other => panic!("Unrecognized geometry: {other:?}"),
    }
}

/// Assert 2 lists contain the same elements in any order (compares the `Debug` representations).
#[track_caller]
pub fn assert_lists_contain_same_elements<T: std::fmt::Debug>(expected: &[T], actual: &[T]) {
    let mut e: Vec<String> = expected.iter().map(|x| format!("{x:?}")).collect();
    let mut a: Vec<String> = actual.iter().map(|x| format!("{x:?}")).collect();
    e.sort();
    a.sort();
    assert_eq!(e, a);
}

/// Geometries rendered into each tile (Java's `Map<TileCoord, Collection<Geometry>>` in renderer tests).
pub type TileGeometries = std::collections::BTreeMap<crate::geo::tile_coord::TileCoord, Vec<Geometry<f64>>>;

/// Builds a [`TileGeometries`] map from `(tile, geometries)` entries.
pub fn tile_geometries(
    entries: impl IntoIterator<Item = (crate::geo::tile_coord::TileCoord, Vec<Geometry<f64>>)>,
) -> TileGeometries {
    entries.into_iter().collect()
}

/// Decodes a vector tile geometry to tile pixel coordinates, panicking on failure (Java `decodeSilently`).
pub fn decode_silently(geom: &crate::vector_tile::VectorGeometry) -> Geometry<f64> {
    geom.decode().unwrap_or_else(|e| panic!("failed to decode {geom:?}: {e:?}"))
}

fn sorted_by_debug(geoms: impl IntoIterator<Item = Geometry<f64>>) -> Vec<Geometry<f64>> {
    let mut result: Vec<(String, Geometry<f64>)> = geoms.into_iter().map(|g| (format!("{g:?}"), g)).collect();
    result.sort_by(|a, b| a.0.cmp(&b.0));
    result.into_iter().map(|(_, g)| g).collect()
}

fn map_tile_features(input: &TileGeometries, f: impl Fn(&Geometry<f64>) -> Geometry<f64>) -> TileGeometries {
    input.iter().map(|(tile, geoms)| (*tile, sorted_by_debug(geoms.iter().map(&f)))).collect()
}

/// Asserts that both maps contain exactly the same geometries in each tile, in any order (Java
/// `assertExactSameFeatures`).
#[track_caller]
pub fn assert_exact_same_features(expected: &TileGeometries, actual: &TileGeometries) {
    assert_eq!(map_tile_features(expected, Clone::clone), map_tile_features(actual, Clone::clone));
}

/// Asserts that both maps contain the same normalized geometries in each tile, in any order (Java
/// `assertSameNormalizedFeatures`).
#[track_caller]
pub fn assert_same_normalized_features(expected: &TileGeometries, actual: &TileGeometries) {
    assert_eq!(map_tile_features(expected, norm), map_tile_features(actual, norm));
}

/// Asserts that both maps contain topologically equivalent geometries in each tile (Java
/// `assertTopologicallyEquivalentFeatures`).
#[track_caller]
pub fn assert_topologically_equivalent_features(expected: &TileGeometries, actual: &TileGeometries) {
    let e = map_tile_features(expected, norm);
    let a = map_tile_features(actual, norm);
    assert_eq!(e.keys().collect::<Vec<_>>(), a.keys().collect::<Vec<_>>(), "tiles differ: {e:?} != {a:?}");
    for (tile, eg) in &e {
        let ag = &a[tile];
        assert_eq!(eg.len(), ag.len(), "{tile:?}: {eg:?} != {ag:?}");
        for (x, y) in eg.iter().zip(ag) {
            assert!(x.relate(y).is_equal_topo(), "{tile:?}: not topologically equal:\n{x:?}\n{y:?}");
        }
    }
}

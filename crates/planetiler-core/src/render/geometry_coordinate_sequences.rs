//! Port of `render/GeometryCoordinateSequences.java`: converts back and forth between [`Geometry`] and [`Groups`]
//! (lists of coordinate sequences) representing linestrings or polygons.
//!
//! The [`Groups`] format is:
//! - for linestrings: `[[linestring], [linestring], ...]` for each linestring in the collection
//! - for polygons: `[[outer ring, inner ring, inner ring], [outer ring, inner ring, ...], ...]` for each polygon in the
//!   multipolygon
//! - for points (output of [`crate::render::tiled_geometry::TiledGeometry::slice_points_into_tiles`]):
//!   `[[point, point, ...]]` where each point is a 1-coordinate sequence.
//!
//! Java mutates the input geometry's coordinate sequences in place when it reverses rings; here the input is never
//! modified (the extracted sequences are copies), and `*_owned` variants of the reassembly functions consume their
//! input to avoid copying.

use geo_types::{Coord, Geometry, LineString, Point, Polygon};

use crate::geo::geo_utils;
use crate::geo::geometry_exception::{GeometryException, GeometryResult};
use crate::geo::geometry_transformer;

/// A list of coordinates (JTS `CoordinateSequence`).
pub type CoordSeq = Vec<Coord<f64>>;
/// Groups of coordinate sequences: one group per linestring, or `[outer, inner...]` rings per polygon.
pub type Groups = Vec<Vec<CoordSeq>>;

/// Port of JTS `Area.ofRingSigned`: positive for clockwise rings (in a y-up coordinate system), negative for
/// counter-clockwise rings, computed the same way as JTS for bit-for-bit identical results.
pub fn ring_signed_area(ring: &[Coord<f64>]) -> f64 {
    let n = ring.len();
    if n < 3 {
        return 0.0;
    }
    let x0 = ring[0].x;
    let mut p1y = ring[0].y;
    let mut p2x = ring[1].x - x0;
    let mut p2y = ring[1].y;
    let mut sum = 0.0;
    for c in &ring[2..] {
        let p0y = p1y;
        let p1x = p2x;
        p1y = p2y;
        p2x = c.x - x0;
        p2y = c.y;
        sum += p1x * (p0y - p2y);
    }
    sum / 2.0
}

/// Returns the coordinate sequences extracted from every linear component in `geom` over a minimum size threshold.
///
/// For linestrings that means all linestrings at least `min_size` long. For polygons that means all lists of
/// `[exterior, interior...]` ring coordinate sequences where the ring area is at least `min_size`. Exterior and
/// interior rings are both returned with counter-clockwise winding (JTS sense).
///
/// # Panics
///
/// If `geom` contains points (Java throws `IllegalArgumentException`).
pub fn extract_groups(geom: &Geometry<f64>, min_size: f64) -> Groups {
    extract_groups_with_rings(geom, min_size, false)
}

/// Same as [`extract_groups`], but when `linear_rings` is true, closed linestrings with at least 4 points are treated
/// like JTS `LinearRing`s: as the shell of a polygon whose area must be at least `min_size`. That is what Java does
/// with the rings left over when simplifying a polygon collapses its shell (JTS keeps them as `LinearRing`s, which geo
/// can't represent).
pub fn extract_groups_with_rings(geom: &Geometry<f64>, min_size: f64, linear_rings: bool) -> Groups {
    let mut result = Vec::new();
    extract_groups_into(geom, &mut result, min_size, linear_rings);
    result
}

fn extract_groups_into(geom: &Geometry<f64>, groups: &mut Groups, min_size: f64, linear_rings: bool) {
    if geometry_transformer::is_empty(geom) {
        return;
    }
    match geom {
        Geometry::GeometryCollection(gc) => {
            for g in &gc.0 {
                extract_groups_into(g, groups, min_size, linear_rings);
            }
        }
        Geometry::MultiPolygon(mp) => {
            for p in &mp.0 {
                if !p.exterior().0.is_empty() {
                    extract_groups_from_polygon(groups, min_size, p);
                }
            }
        }
        Geometry::MultiLineString(ml) => {
            for l in &ml.0 {
                if !l.0.is_empty() {
                    extract_line(groups, min_size, l, linear_rings);
                }
            }
        }
        Geometry::Polygon(p) => extract_groups_from_polygon(groups, min_size, p),
        Geometry::Rect(r) => extract_groups_from_polygon(groups, min_size, &r.to_polygon()),
        Geometry::Triangle(t) => extract_groups_from_polygon(groups, min_size, &t.to_polygon()),
        Geometry::LineString(l) => extract_line(groups, min_size, l, linear_rings),
        Geometry::Line(l) => extract_line(groups, min_size, &LineString(vec![l.start, l.end]), false),
        Geometry::Point(_) | Geometry::MultiPoint(_) => {
            panic!("unrecognized geometry type: {:?}", crate::geo::geometry_type::GeometryType::type_of(geom))
        }
    }
}

fn extract_line(groups: &mut Groups, min_size: f64, line: &LineString<f64>, linear_rings: bool) {
    if linear_rings && line.0.len() >= 4 && line.0.first() == line.0.last() {
        // Java: extractGroups(JTS_FACTORY.createPolygon(linearRing), groups, minSize)
        extract_groups_from_polygon(groups, min_size, &Polygon::new(line.clone(), vec![]));
    } else if line_length(&line.0) >= min_size {
        groups.push(vec![line.0.clone()]);
    }
}

/// JTS `Length.ofLine`.
fn line_length(coords: &[Coord<f64>]) -> f64 {
    let mut len = 0.0;
    for w in coords.windows(2) {
        let dx = w[1].x - w[0].x;
        let dy = w[1].y - w[0].y;
        len += (dx * dx + dy * dy).sqrt();
    }
    len
}

fn extract_groups_from_polygon(groups: &mut Groups, min_area: f64, polygon: &Polygon<f64>) {
    let outer_coords = &polygon.exterior().0;
    let outer_area = ring_signed_area(outer_coords);
    if outer_area.abs() >= min_area {
        let mut outer = outer_coords.clone();
        // ensure CCW winding
        if outer_area > 0.0 {
            outer.reverse();
        }
        let mut group = Vec::with_capacity(1 + polygon.interiors().len());
        group.push(outer);
        for inner_ring in polygon.interiors() {
            let inner_area = ring_signed_area(&inner_ring.0);
            if inner_area.abs() >= min_area {
                let mut inner = inner_ring.0.clone();
                // use CCW winding for inner rings as well, subsequent processing will reverse them
                if inner_area > 0.0 {
                    inner.reverse();
                }
                group.push(inner);
            }
        }
        groups.push(group);
    }
}

/// Returns a linestring or multilinestring containing all coordinate sequences in `geoms` with more than one point.
pub fn reassemble_line_strings(geoms: &Groups) -> Geometry<f64> {
    let line_strings = geoms.iter().flatten().filter(|s| s.len() > 1).map(|s| LineString(s.clone())).collect();
    geo_utils::combine_line_strings(line_strings)
}

/// Like [`reassemble_line_strings`] but consumes the input.
pub fn reassemble_line_strings_owned(geoms: Groups) -> Geometry<f64> {
    let line_strings = geoms.into_iter().flatten().filter(|s| s.len() > 1).map(LineString).collect();
    geo_utils::combine_line_strings(line_strings)
}

/// Returns a polygon or multipolygon from all groups of exterior/interior rings in `groups`, reversing inner rings.
///
/// Fails if rings are not closed or have too few points.
pub fn reassemble_polygons(groups: &Groups) -> GeometryResult<Geometry<f64>> {
    reassemble_polygons_owned(groups.clone())
}

/// Like [`reassemble_polygons`] but consumes the input.
pub fn reassemble_polygons_owned(groups: Groups) -> GeometryResult<Geometry<f64>> {
    if groups.len() == 1 {
        Ok(Geometry::Polygon(reassemble_polygon(groups.into_iter().next().unwrap())?))
    } else {
        let polygons = groups.into_iter().map(reassemble_polygon).collect::<GeometryResult<Vec<_>>>()?;
        Ok(Geometry::MultiPolygon(geo_types::MultiPolygon(polygons)))
    }
}

/// JTS `GeometryFactory.createLinearRing` validation.
fn check_linear_ring(seq: &[Coord<f64>]) -> GeometryResult<()> {
    if seq.is_empty() {
        return Ok(());
    }
    if seq.first() != seq.last() {
        return Err(GeometryException::new("reassemble_polygon_failed", "Could not build polygon")
            .with_cause("Points of LinearRing do not form a closed linestring"));
    }
    if seq.len() < 4 {
        return Err(GeometryException::new("reassemble_polygon_failed", "Could not build polygon")
            .with_cause(format!("Invalid number of points in LinearRing (found {} - must be 0 or >= 4)", seq.len())));
    }
    Ok(())
}

fn reassemble_polygon(group: Vec<CoordSeq>) -> GeometryResult<Polygon<f64>> {
    let mut iter = group.into_iter();
    let first = iter.next().unwrap_or_default();
    check_linear_ring(&first)?;
    let mut rest = Vec::with_capacity(iter.len());
    for mut seq in iter {
        seq.reverse();
        check_linear_ring(&seq)?;
        rest.push(LineString(seq));
    }
    if first.is_empty() && !rest.is_empty() {
        return Err(GeometryException::new("reassemble_polygon_failed", "Could not build polygon")
            .with_cause("shell is empty but holes are not"));
    }
    Ok(Polygon::new(LineString(first), rest))
}

/// Returns a point or multipoint from all 1-coordinate sequences in `result`.
pub fn reassemble_points(result: &Groups) -> Geometry<f64> {
    let points = result.iter().flatten().filter(|s| s.len() == 1).map(|s| Point(s[0])).collect();
    geo_utils::combine_points(points)
}

/// Like [`reassemble_points`] but consumes the input.
pub fn reassemble_points_owned(result: Groups) -> Geometry<f64> {
    reassemble_points(&result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::test_utils::*;

    #[test]
    fn signed_area_matches_jts_sign() {
        // CCW in y-up coordinates -> negative in JTS
        let ccw = new_coordinate_list(&[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 0.0]);
        assert_eq!(-1.0, ring_signed_area(&ccw));
        let mut cw = ccw.clone();
        cw.reverse();
        assert_eq!(1.0, ring_signed_area(&cw));
    }

    #[test]
    fn extract_polygon_makes_rings_ccw_and_filters_small() {
        let poly = new_polygon_with_holes(
            rectangle_coord_list(0.0, 10.0),
            vec![rectangle_coord_list(1.0, 2.0), rectangle_coord_list(3.0, 8.0)],
        );
        let groups = extract_groups(&poly.into(), 2.0);
        assert_eq!(1, groups.len());
        assert_eq!(2, groups[0].len());
        for ring in &groups[0] {
            assert!(ring_signed_area(ring) < 0.0);
        }
        let poly = reassemble_polygons(&groups).unwrap();
        let Geometry::Polygon(p) = poly else { panic!() };
        // inner ring gets reversed back to CW
        assert!(ring_signed_area(&p.interiors()[0].0) > 0.0);
    }

    #[test]
    fn extract_lines() {
        let g: Geometry<f64> = new_multi_line_string(vec![
            new_line_string(&[0.0, 0.0, 1.0, 0.0]),
            new_line_string(&[0.0, 0.0, 10.0, 0.0]),
        ])
        .into();
        let groups = extract_groups(&g, 2.0);
        assert_eq!(1, groups.len());
        assert_eq!(Geometry::LineString(new_line_string(&[0.0, 0.0, 10.0, 0.0])), reassemble_line_strings(&groups));
        assert_eq!(Geometry::MultiLineString(geo_types::MultiLineString(vec![])), reassemble_line_strings(&vec![]));
    }

    #[test]
    fn reassemble_invalid_ring_fails() {
        assert!(reassemble_polygons(&vec![vec![new_coordinate_list(&[0.0, 0.0, 1.0, 1.0, 0.0, 0.0])]]).is_err());
        assert!(
            reassemble_polygons(&vec![vec![new_coordinate_list(&[0.0, 0.0, 1.0, 1.0, 1.0, 0.0, 0.5, 0.0])]]).is_err()
        );
    }

    #[test]
    fn reassemble_points_single_and_multi() {
        let groups = vec![vec![vec![Coord { x: 1.0, y: 2.0 }]]];
        assert_eq!(Geometry::Point(new_point(1.0, 2.0)), reassemble_points(&groups));
        let groups = vec![vec![vec![Coord { x: 1.0, y: 2.0 }], vec![Coord { x: 3.0, y: 4.0 }]]];
        assert_eq!(
            Geometry::MultiPoint(new_multi_point(vec![new_point(1.0, 2.0), new_point(3.0, 4.0)])),
            reassemble_points(&groups)
        );
    }
}

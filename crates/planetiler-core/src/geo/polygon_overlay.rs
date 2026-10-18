//! Polygon repair, precision reduction and boolean operations built on `i_overlay`'s integer overlay engine.
//!
//! These replace the JTS operations planetiler relies on:
//!
//! - [`buffer_zero`]: JTS `geom.buffer(0)`. Each polygon's shell is oriented to have positive signed area and its
//!   holes negative, then everything is resolved with a *positive* winding fill rule. Like JTS, a self-intersecting
//!   "bow-tie" keeps only the lobes that wind the same way as the overall ring.
//! - [`geometry_fixer`]: JTS `GeometryFixer.fix`. Rings are resolved with a *non-zero* fill (both lobes of a bow-tie
//!   are kept), holes are subtracted from their shell and the resulting polygons are unioned.
//! - [`reduce_precision`]: JTS `GeometryPrecisionReducer.reduce`. Coordinates are rounded to the precision grid and
//!   the result is re-noded on that grid (intersection points are rounded to the grid as well), equivalent to JTS's
//!   snap-rounding overlay. The output is always valid, so unlike JTS this never fails.
//! - [`union`], [`difference`], [`intersection`]: polygon boolean operations.
//!
//! Output polygons follow the JTS overlay convention: shells clockwise and holes counter-clockwise (in the usual
//! y-up sense used by `geo::Winding`). Output is OGC-valid (holes that touch the shell at a point are separate holes,
//! not self-touching shells).

use geo::Area;
use geo_types::{Coord, Geometry, LineString, MultiPolygon, Point, Polygon};
use i_overlay::core::fill_rule::FillRule;
use i_overlay::core::integer::OverlayInt;
use i_overlay::core::overlay::{ContourDirection, IntOverlayOptions, Overlay, ShapeType};
use i_overlay::core::overlay_rule::OverlayRule;
use i_overlay::core::solver::Solver;
use i_overlay::i_float::int::point::IntPoint;
use i_overlay::i_shape::int::shape::IntShapes;

use crate::geo::geometry_exception::{GeometryException, GeometryResult};
use crate::geo::geometry_transformer::build_geometry;
use crate::geo::precision_model::{PrecisionModel, java_round};

/// Maps between floating point coordinates and the integer grid used by the overlay engine.
#[derive(Clone, Copy, Debug)]
struct Grid {
    scale: f64,
    /// Round to the nearest grid point with Java semantics (precision reduction) instead of exact scaling.
    round: bool,
}

/// Largest absolute integer coordinate allowed for each engine (with a safety margin below the documented limits).
const I32_LIMIT: f64 = (1u64 << 28) as f64;
const I64_LIMIT: f64 = (1u64 << 59) as f64;

impl Grid {
    #[inline]
    fn to_int(self, c: Coord<f64>) -> (i64, i64) {
        if self.round {
            (java_round(c.x * self.scale) as i64, java_round(c.y * self.scale) as i64)
        } else {
            ((c.x * self.scale).round() as i64, (c.y * self.scale).round() as i64)
        }
    }

    #[inline]
    fn to_float(self, x: i64, y: i64) -> Coord<f64> {
        Coord { x: x as f64 / self.scale, y: y as f64 / self.scale }
    }
}

fn max_abs_coord<'a>(polys: impl Iterator<Item = &'a Polygon<f64>>) -> f64 {
    let mut max = 0f64;
    for p in polys {
        for c in p.exterior().0.iter().chain(p.interiors().iter().flat_map(|r| r.0.iter())) {
            max = max.max(c.x.abs()).max(c.y.abs());
        }
    }
    max
}

/// A grid that represents coordinates with (nearly) full double precision.
fn exact_grid(max_abs: f64) -> Grid {
    if max_abs == 0.0 || !max_abs.is_finite() {
        return Grid { scale: 1.0, round: false };
    }
    // largest power of 2 such that max_abs * scale <= 2^52 (exactly representable, below the i64 limit)
    let exp = 52 - (max_abs.log2().ceil() as i32);
    Grid { scale: 2f64.powi(exp), round: false }
}

/// A contour in integer coordinates, with the signed area (positive = counter-clockwise) of the source ring.
type Contour = Vec<(i64, i64)>;

fn ring_contour(ring: &LineString<f64>, grid: Grid) -> Contour {
    let mut out: Contour = Vec::with_capacity(ring.0.len());
    for c in &ring.0 {
        let p = grid.to_int(*c);
        if out.last() != Some(&p) {
            out.push(p);
        }
    }
    if out.len() > 1 && out.first() == out.last() {
        out.pop();
    }
    out
}

/// Twice the signed area of an integer contour (positive = counter-clockwise).
fn signed_area2(contour: &Contour) -> i128 {
    let n = contour.len();
    let mut sum: i128 = 0;
    for i in 0..n {
        let (x1, y1) = contour[i];
        let (x2, y2) = contour[(i + 1) % n];
        sum += x1 as i128 * y2 as i128 - x2 as i128 * y1 as i128;
    }
    sum
}

fn oriented(mut contour: Contour, ccw: bool) -> Contour {
    if (signed_area2(&contour) > 0) != ccw {
        contour.reverse();
    }
    contour
}

/// A set of contours to feed into the overlay engine.
#[derive(Default)]
struct Contours {
    subject: Vec<Contour>,
    clip: Vec<Contour>,
}

fn run_overlay<I: OverlayInt + TryFrom<i64> + Into<i64>>(
    contours: &Contours,
    rule: OverlayRule,
    fill: FillRule,
    preserve_collinear: bool,
) -> IntShapes<I> {
    let capacity = contours.subject.iter().chain(&contours.clip).map(|c| c.len()).sum();
    let options = IntOverlayOptions {
        preserve_input_collinear: preserve_collinear,
        output_direction: ContourDirection::Clockwise,
        preserve_output_collinear: preserve_collinear,
        ogc: true,
        ..IntOverlayOptions::keep_all_points()
    };
    let solver = Solver { multithreading: None, ..Solver::default() };
    let mut overlay: Overlay<I> = Overlay::new_custom(capacity, options, solver);
    let conv = |c: &Contour| -> Vec<IntPoint<I>> {
        c.iter()
            .map(|(x, y)| {
                IntPoint::new(I::try_from(*x).ok().expect("in range"), I::try_from(*y).ok().expect("in range"))
            })
            .collect()
    };
    for c in &contours.subject {
        if c.len() >= 3 {
            overlay.add_contour(&conv(c), ShapeType::Subject);
        }
    }
    for c in &contours.clip {
        if c.len() >= 3 {
            overlay.add_contour(&conv(c), ShapeType::Clip);
        }
    }
    overlay.overlay(rule, fill)
}

fn shapes_to_polygons<I: OverlayInt + Into<i64>>(shapes: IntShapes<I>, grid: Grid) -> Vec<Polygon<f64>> {
    let ring = |contour: &[IntPoint<I>]| -> LineString<f64> {
        let mut coords: Vec<Coord<f64>> = contour.iter().map(|p| grid.to_float(p.x.into(), p.y.into())).collect();
        if let Some(first) = coords.first().copied() {
            coords.push(first);
        }
        LineString(coords)
    };
    shapes
        .into_iter()
        .filter(|shape| !shape.is_empty())
        .map(|shape| Polygon::new(ring(&shape[0]), shape[1..].iter().map(|c| ring(c)).collect()))
        .collect()
}

fn overlay(
    contours: &Contours,
    grid: Grid,
    max_int: f64,
    rule: OverlayRule,
    fill: FillRule,
    preserve_collinear: bool,
) -> GeometryResult<Vec<Polygon<f64>>> {
    if !max_int.is_finite() {
        return Err(GeometryException::new("overlay_invalid_coordinate", "non-finite coordinate"));
    }
    if max_int <= I32_LIMIT {
        Ok(shapes_to_polygons(run_overlay::<i32>(contours, rule, fill, preserve_collinear), grid))
    } else if max_int <= I64_LIMIT {
        Ok(shapes_to_polygons(run_overlay::<i64>(contours, rule, fill, preserve_collinear), grid))
    } else {
        Err(GeometryException::new("overlay_coordinate_out_of_range", "coordinate out of range for overlay"))
    }
}

/// Collect the polygons from a geometry.
pub fn polygons_of(geom: &Geometry<f64>) -> Vec<&Polygon<f64>> {
    let mut out = Vec::new();
    collect_polygons(geom, &mut out);
    out
}

fn collect_polygons<'a>(geom: &'a Geometry<f64>, out: &mut Vec<&'a Polygon<f64>>) {
    match geom {
        Geometry::Polygon(p) => out.push(p),
        Geometry::MultiPolygon(mp) => out.extend(mp.0.iter()),
        Geometry::GeometryCollection(gc) => gc.0.iter().for_each(|g| collect_polygons(g, out)),
        _ => {}
    }
}

/// Build a polygonal geometry from a list of polygons: empty polygon, single polygon or multipolygon.
pub fn to_polygonal(mut polys: Vec<Polygon<f64>>) -> Geometry<f64> {
    match polys.len() {
        0 => Geometry::Polygon(Polygon::new(LineString(vec![]), vec![])),
        1 => Geometry::Polygon(polys.pop().unwrap()),
        _ => Geometry::MultiPolygon(MultiPolygon(polys)),
    }
}

/// Shells counter-clockwise and holes clockwise, the way the positive fill rule expects them.
fn positive_contours(polys: &[&Polygon<f64>], grid: Grid) -> Vec<Contour> {
    let mut out = Vec::new();
    for poly in polys {
        if poly.exterior().0.is_empty() {
            continue;
        }
        out.push(oriented(ring_contour(poly.exterior(), grid), true));
        for hole in poly.interiors() {
            out.push(oriented(ring_contour(hole, grid), false));
        }
    }
    out
}

/// Equivalent of JTS `geom.buffer(0)` for polygonal geometries: resolves self-intersections and overlaps. Points and
/// lines produce an empty polygon.
pub fn buffer_zero(geom: &Geometry<f64>) -> GeometryResult<Geometry<f64>> {
    let polys = polygons_of(geom);
    let grid = exact_grid(max_abs_coord(polys.iter().copied()));
    let contours = Contours { subject: positive_contours(&polys, grid), clip: vec![] };
    let max_int = max_abs_coord(polys.iter().copied()) * grid.scale;
    Ok(to_polygonal(overlay(&contours, grid, max_int, OverlayRule::Subject, FillRule::Positive, true)?))
}

/// Equivalent of JTS `GeometryPrecisionReducer.reduce(geom, precisionModel)`.
///
/// Polygonal input (which should be valid) is rounded to the grid and re-noded so the result is valid; collapsed
/// parts are removed. Points and lines are rounded pointwise (repeated points removed, collapsed lines dropped).
// TODO(port): JTS snap-rounds lines too (noding intersections); lines are only rounded pointwise here.
pub fn reduce_precision(geom: &Geometry<f64>, pm: &PrecisionModel) -> GeometryResult<Geometry<f64>> {
    let scale = match pm {
        PrecisionModel::Floating => return Ok(geom.clone()),
        PrecisionModel::Fixed { scale } => *scale,
    };
    let grid = Grid { scale, round: true };
    let polys = polygons_of(geom);
    let polygonal = matches!(geom, Geometry::Polygon(_) | Geometry::MultiPolygon(_));
    let reduced_polys = if polys.is_empty() {
        vec![]
    } else {
        let contours = Contours { subject: positive_contours(&polys, grid), clip: vec![] };
        let max_int = max_abs_coord(polys.iter().copied()) * scale + 1.0;
        overlay(&contours, grid, max_int, OverlayRule::Subject, FillRule::Positive, true)?
    };
    if polygonal {
        return Ok(to_polygonal(reduced_polys));
    }
    let mut others = Vec::new();
    collect_non_polygonal_reduced(geom, pm, &mut others);
    let mut all: Vec<Geometry<f64>> = reduced_polys.into_iter().map(Geometry::Polygon).collect();
    all.extend(others);
    Ok(build_geometry(all))
}

fn collect_non_polygonal_reduced(geom: &Geometry<f64>, pm: &PrecisionModel, out: &mut Vec<Geometry<f64>>) {
    let reduce_line = |ls: &LineString<f64>| -> Option<LineString<f64>> {
        let mut coords: Vec<Coord<f64>> = Vec::with_capacity(ls.0.len());
        for c in &ls.0 {
            let c = pm.make_precise_coord(*c);
            if coords.last() != Some(&c) {
                coords.push(c);
            }
        }
        (coords.len() >= 2).then_some(LineString(coords))
    };
    match geom {
        Geometry::Point(p) => out.push(Geometry::Point(Point(pm.make_precise_coord(p.0)))),
        Geometry::MultiPoint(mp) => {
            out.extend(mp.0.iter().map(|p| Geometry::Point(Point(pm.make_precise_coord(p.0)))));
        }
        Geometry::Line(l) => out.extend(reduce_line(&(*l).into()).map(Geometry::LineString)),
        Geometry::LineString(ls) => out.extend(reduce_line(ls).map(Geometry::LineString)),
        Geometry::MultiLineString(mls) => out.extend(mls.0.iter().filter_map(reduce_line).map(Geometry::LineString)),
        Geometry::GeometryCollection(gc) => gc.0.iter().for_each(|g| collect_non_polygonal_reduced(g, pm, out)),
        _ => {}
    }
}

/// Equivalent of JTS `GeometryFixer.fix(geom)`: repairs invalid polygons keeping all area enclosed by each ring
/// (non-zero winding), subtracting holes and unioning overlapping polygons. Lines have repeated points removed and
/// collapsed lines become points; non-finite coordinates are dropped.
pub fn geometry_fixer(geom: &Geometry<f64>) -> GeometryResult<Geometry<f64>> {
    let polys = polygons_of(geom);
    let mut fixed_polys = Vec::new();
    if !polys.is_empty() {
        let max = max_abs_coord(polys.iter().copied());
        let grid = exact_grid(max);
        let max_int = max * grid.scale;
        let mut parts: Vec<Polygon<f64>> = Vec::new();
        for poly in &polys {
            if poly.exterior().0.is_empty() {
                continue;
            }
            let contours = Contours {
                subject: vec![ring_contour(poly.exterior(), grid)],
                clip: poly.interiors().iter().map(|h| ring_contour(h, grid)).collect(),
            };
            parts.extend(overlay(&contours, grid, max_int, OverlayRule::Difference, FillRule::NonZero, true)?);
        }
        fixed_polys = if parts.len() > 1 {
            let refs: Vec<&Polygon<f64>> = parts.iter().collect();
            let contours = Contours { subject: positive_contours(&refs, grid), clip: vec![] };
            overlay(&contours, grid, max_int, OverlayRule::Subject, FillRule::Positive, true)?
        } else {
            parts
        };
    }
    if matches!(geom, Geometry::Polygon(_) | Geometry::MultiPolygon(_)) {
        return Ok(to_polygonal(fixed_polys));
    }
    let mut others = Vec::new();
    collect_fixed_non_polygonal(geom, &mut others);
    let mut all: Vec<Geometry<f64>> = fixed_polys.into_iter().map(Geometry::Polygon).collect();
    all.extend(others);
    Ok(build_geometry(all))
}

fn collect_fixed_non_polygonal(geom: &Geometry<f64>, out: &mut Vec<Geometry<f64>>) {
    let finite = |c: &Coord<f64>| c.x.is_finite() && c.y.is_finite();
    let fix_line = |ls: &LineString<f64>| -> Option<Geometry<f64>> {
        let mut coords: Vec<Coord<f64>> = Vec::with_capacity(ls.0.len());
        for c in ls.0.iter().filter(|c| finite(c)) {
            if coords.last() != Some(c) {
                coords.push(*c);
            }
        }
        match coords.len() {
            0 => None,
            1 => Some(Geometry::Point(Point(coords[0]))),
            _ => Some(Geometry::LineString(LineString(coords))),
        }
    };
    match geom {
        Geometry::Point(p) if finite(&p.0) => out.push(Geometry::Point(*p)),
        Geometry::MultiPoint(mp) => out.extend(mp.0.iter().filter(|p| finite(&p.0)).map(|p| Geometry::Point(*p))),
        Geometry::Line(l) => out.extend(fix_line(&(*l).into())),
        Geometry::LineString(ls) => out.extend(fix_line(ls)),
        Geometry::MultiLineString(mls) => out.extend(mls.0.iter().filter_map(fix_line)),
        Geometry::GeometryCollection(gc) => gc.0.iter().for_each(|g| collect_fixed_non_polygonal(g, out)),
        _ => {}
    }
}

fn boolean_op(a: &Geometry<f64>, b: &Geometry<f64>, rule: OverlayRule) -> GeometryResult<Geometry<f64>> {
    let pa = polygons_of(a);
    let pb = polygons_of(b);
    let max = max_abs_coord(pa.iter().chain(pb.iter()).copied());
    let grid = exact_grid(max);
    let contours = Contours { subject: positive_contours(&pa, grid), clip: positive_contours(&pb, grid) };
    Ok(to_polygonal(overlay(&contours, grid, max * grid.scale, rule, FillRule::Positive, true)?))
}

/// Union of all polygons in `a` and `b` (non-polygonal components are ignored).
pub fn union(a: &Geometry<f64>, b: &Geometry<f64>) -> GeometryResult<Geometry<f64>> {
    boolean_op(a, b, OverlayRule::Union)
}

/// Union of all polygons in `geom` (JTS `geom.union()` for polygonal geometries).
pub fn union_all(geom: &Geometry<f64>) -> GeometryResult<Geometry<f64>> {
    let polys = polygons_of(geom);
    let max = max_abs_coord(polys.iter().copied());
    let grid = exact_grid(max);
    let contours = Contours { subject: positive_contours(&polys, grid), clip: vec![] };
    Ok(to_polygonal(overlay(&contours, grid, max * grid.scale, OverlayRule::Subject, FillRule::NonZero, true)?))
}

/// Polygons in `a` minus polygons in `b`.
pub fn difference(a: &Geometry<f64>, b: &Geometry<f64>) -> GeometryResult<Geometry<f64>> {
    boolean_op(a, b, OverlayRule::Difference)
}

/// Intersection of the polygons in `a` and `b`.
pub fn intersection(a: &Geometry<f64>, b: &Geometry<f64>) -> GeometryResult<Geometry<f64>> {
    boolean_op(a, b, OverlayRule::Intersect)
}

/// Unsigned area of a polygonal geometry.
pub fn area(geom: &Geometry<f64>) -> f64 {
    geom.unsigned_area()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::is_valid::is_valid;
    use crate::geo::test_utils::*;
    use geo::Winding;

    fn poly(coords: &[f64]) -> Geometry<f64> {
        new_polygon(coords).into()
    }

    #[test]
    fn buffer_zero_valid_polygon_unchanged() {
        let result = buffer_zero(&rectangle(0.0, 1.0).into()).unwrap();
        assert_same_normalized_feature(&rectangle(0.0, 1.0).into(), &result);
        assert!(is_valid(&result));
        if let Geometry::Polygon(p) = &result {
            assert!(p.exterior().is_cw());
        } else {
            panic!("{result:?}");
        }
    }

    #[test]
    fn buffer_zero_keeps_one_lobe_of_bowtie() {
        // lobes of area 0.75 and 6.75 winding in opposite directions
        let bowtie = poly(&[0.0, 0.0, 6.0, 3.0, 6.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
        let result = buffer_zero(&bowtie).unwrap();
        assert!(is_valid(&result));
        assert!((result.unsigned_area() - 6.75).abs() < 1e-9, "{result:?}");
    }

    #[test]
    fn fixer_keeps_both_lobes_of_bowtie() {
        let bowtie = poly(&[0.0, 0.0, 6.0, 3.0, 6.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
        let result = geometry_fixer(&bowtie).unwrap();
        assert!(is_valid(&result));
        assert!((result.unsigned_area() - 7.5).abs() < 1e-9, "{result:?}");
    }

    #[test]
    fn hole_subtracted() {
        let p = new_polygon_with_holes(rectangle_coord_list(0.0, 10.0), vec![rectangle_coord_list(1.0, 2.0)]);
        let result = buffer_zero(&p.clone().into()).unwrap();
        assert!((result.unsigned_area() - 99.0).abs() < 1e-9);
        let result = geometry_fixer(&p.into()).unwrap();
        assert!((result.unsigned_area() - 99.0).abs() < 1e-9);
        assert!(is_valid(&result));
    }

    #[test]
    fn overlapping_union() {
        let mp: Geometry<f64> = new_multi_polygon(vec![rectangle(0.0, 2.0), rectangle(1.0, 3.0)]).into();
        let result = buffer_zero(&mp).unwrap();
        assert!((result.unsigned_area() - 7.0).abs() < 1e-9);
        assert!(is_valid(&result));
        assert!((union_all(&mp).unwrap().unsigned_area() - 7.0).abs() < 1e-9);
    }

    #[test]
    fn reduce_precision_rounds_and_fixes() {
        let pm = PrecisionModel::fixed(1.0);
        // thin sliver collapses
        let sliver = poly(&[0.0, 0.0, 10.0, 0.0, 10.0, 0.2, 0.0, 0.2, 0.0, 0.0]);
        let result = reduce_precision(&sliver, &pm).unwrap();
        assert_eq!(0.0, result.unsigned_area());
        let p = poly(&[0.1, 0.1, 10.2, 0.0, 9.9, 10.1, 0.0, 9.8, 0.1, 0.1]);
        let result = reduce_precision(&p, &pm).unwrap();
        assert_same_normalized_feature(&rectangle(0.0, 10.0).into(), &result);
    }

    #[test]
    fn boolean_ops() {
        let a: Geometry<f64> = rectangle(0.0, 2.0).into();
        let b: Geometry<f64> = rectangle(1.0, 3.0).into();
        assert!((union(&a, &b).unwrap().unsigned_area() - 7.0).abs() < 1e-9);
        assert!((intersection(&a, &b).unwrap().unsigned_area() - 1.0).abs() < 1e-9);
        assert!((difference(&a, &b).unwrap().unsigned_area() - 3.0).abs() < 1e-9);
    }

    #[test]
    fn hole_touching_shell_is_ogc_valid() {
        // a ring that touches itself, which should become a polygon with a hole touching the shell
        let p = poly(&[0.0, 0.0, 4.0, 0.0, 4.0, 4.0, 0.0, 4.0, 0.0, 2.0, 2.0, 3.0, 2.0, 1.0, 0.0, 2.0, 0.0, 0.0]);
        let result = buffer_zero(&p).unwrap();
        assert!(is_valid(&result), "{result:?}");
        assert!((result.unsigned_area() - 14.0).abs() < 1e-9);
    }
}

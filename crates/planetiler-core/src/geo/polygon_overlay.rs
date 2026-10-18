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

/// Largest absolute integer coordinate allowed for each engine: `i_overlay` supports `2^(N - 2)` for an N-bit
/// engine and uses `2^(N - 3)` itself for its float adapters; the i32 limit has an extra safety margin.
const I32_LIMIT: f64 = (1u64 << 28) as f64;
const I64_LIMIT: f64 = (1u64 << 61) as f64;

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
///
/// The largest power of 2 such that `max_abs * scale <= 2^61` (`I64_LIMIT`). Every coordinate with magnitude at
/// least `max_abs / 2^9` is represented exactly (its ulp is a multiple of the grid unit), e.g. everything >= 1 in
/// magnitude for tile coordinates up to 512. This matters where edges are collinear or a vertex lies on another edge
/// in double precision: rounding to a coarser grid can turn that into a spurious crossing or move the vertex to the
/// other side of the edge, which changes which vertices end up in the output (JTS works on the doubles).
fn exact_grid(max_abs: f64) -> Grid {
    if max_abs == 0.0 || !max_abs.is_finite() {
        return Grid { scale: 1.0, round: false };
    }
    let mut exp = 61 - (max_abs.log2().ceil() as i32);
    // log2 can round down to an integer for values just above a power of 2
    while max_abs * 2f64.powi(exp) > I64_LIMIT {
        exp -= 1;
    }
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
    let polys = shapes
        .into_iter()
        .filter(|shape| !shape.is_empty())
        .map(|shape| Polygon::new(ring(&shape[0]), shape[1..].iter().map(|c| ring(c)).collect()))
        .collect();
    rebind_misplaced_holes(polys)
}

/// `(min_x, min_y, max_x, max_y)` of a ring.
fn ring_bounds(ring: &LineString<f64>) -> (f64, f64, f64, f64) {
    ring.0.iter().fold((f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY), |(a, b, c, d), p| {
        (a.min(p.x), b.min(p.y), c.max(p.x), d.max(p.y))
    })
}

fn ring_area_abs(ring: &LineString<f64>) -> f64 {
    (ring.0.windows(2).map(|w| w[0].x * w[1].y - w[1].x * w[0].y).sum::<f64>() / 2.0).abs()
}

fn bounds_cover(outer: (f64, f64, f64, f64), inner: (f64, f64, f64, f64)) -> bool {
    outer.0 <= inner.0 && outer.1 <= inner.1 && outer.2 >= inner.2 && outer.3 >= inner.3
}

/// Works around `i_overlay` (9.1, OGC mode) occasionally attaching holes to the wrong shell of a multi-shape result
/// (seen on complex shorelines, e.g. `mdshore.wkb` / `njshore.wkb` in `PlanetilerTests`), which makes the output an
/// invalid multipolygon. If any hole is not within its shell's envelope, every hole is re-bound to the smallest shell
/// that contains it.
fn rebind_misplaced_holes(polys: Vec<Polygon<f64>>) -> Vec<Polygon<f64>> {
    if polys.len() < 2 {
        return polys;
    }
    let shell_bounds: Vec<_> = polys.iter().map(|p| ring_bounds(p.exterior())).collect();
    let misplaced =
        polys.iter().zip(&shell_bounds).any(|(p, &sb)| p.interiors().iter().any(|h| !bounds_cover(sb, ring_bounds(h))));
    if !misplaced {
        return polys;
    }
    let shell_areas: Vec<f64> = polys.iter().map(|p| ring_area_abs(p.exterior())).collect();
    let mut shells = Vec::with_capacity(polys.len());
    let mut holes = Vec::new();
    for (i, p) in polys.into_iter().enumerate() {
        let (exterior, interiors) = p.into_inner();
        shells.push(exterior);
        holes.extend(interiors.into_iter().map(|h| (i, h)));
    }
    let mut holes_for_shell: Vec<Vec<LineString<f64>>> = vec![Vec::new(); shells.len()];
    for (original, hole) in holes {
        let hb = ring_bounds(&hole);
        let parent = (0..shells.len())
            .filter(|&s| bounds_cover(shell_bounds[s], hb))
            .filter(|&s| {
                let shell = &shells[s].0;
                hole.0
                    .iter()
                    .map(|c| crate::geo::is_valid::locate_in_ring(*c, shell))
                    .find(|l| *l != crate::geo::is_valid::Location::Boundary)
                    == Some(crate::geo::is_valid::Location::Interior)
            })
            .min_by(|&a, &b| shell_areas[a].total_cmp(&shell_areas[b]))
            .unwrap_or(original);
        holes_for_shell[parent].push(hole);
    }
    shells.into_iter().zip(holes_for_shell).map(|(shell, holes)| Polygon::new(shell, holes)).collect()
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

/// A JTS snap-rounding hot pixel: the unit square (in grid units) centered on a rounded vertex, with open top and
/// right sides.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct HotPixel {
    x: i64,
    y: i64,
}

/// Hot pixels sorted by `(x, y)` and by `(y, x)`, queried by envelope. Each query scans whichever of the two sorted
/// ranges (pixels with x in range, or with y in range) is shorter. Tile polygons have a few hundred vertices, where
/// this is much cheaper than building an R-tree per call.
struct HotPixelIndex {
    by_x: Vec<HotPixel>,
    by_y: Vec<HotPixel>,
}

impl HotPixelIndex {
    /// `pixels` must be sorted by `(x, y)` and deduplicated.
    fn new(pixels: &[(i64, i64)]) -> HotPixelIndex {
        let by_x: Vec<HotPixel> = pixels.iter().map(|&(x, y)| HotPixel { x, y }).collect();
        let mut by_y = by_x.clone();
        by_y.sort_unstable_by_key(|p| (p.y, p.x));
        HotPixelIndex { by_x, by_y }
    }

    /// Calls `f` for every pixel inside (or on the boundary of) the envelope `[min, max]`.
    #[inline]
    fn for_each_in(&self, min: (f64, f64), max: (f64, f64), mut f: impl FnMut(&HotPixel)) {
        let x_start = self.by_x.partition_point(|p| (p.x as f64) < min.0);
        let x_end = self.by_x.partition_point(|p| (p.x as f64) <= max.0);
        let y_start = self.by_y.partition_point(|p| (p.y as f64) < min.1);
        let y_end = self.by_y.partition_point(|p| (p.y as f64) <= max.1);
        if x_end.saturating_sub(x_start) <= y_end.saturating_sub(y_start) {
            for p in &self.by_x[x_start..x_end.max(x_start)] {
                let y = p.y as f64;
                if min.1 <= y && y <= max.1 {
                    f(p);
                }
            }
        } else {
            for p in &self.by_y[y_start..y_end.max(y_start)] {
                let x = p.x as f64;
                if min.0 <= x && x <= max.0 {
                    f(p);
                }
            }
        }
    }
}

/// JTS `CGAlgorithmsDD.orientationIndex` sign (1 = left/counter-clockwise, -1 = right, 0 = collinear) using robust
/// predicates.
fn orientation_index(p: (f64, f64), q: (f64, f64), r: (f64, f64)) -> i8 {
    use geo::kernels::{Kernel, Orientation, RobustKernel};
    match RobustKernel::orient2d(Coord { x: p.0, y: p.1 }, Coord { x: q.0, y: q.1 }, Coord { x: r.0, y: r.1 }) {
        Orientation::CounterClockwise => 1,
        Orientation::Clockwise => -1,
        Orientation::Collinear => 0,
    }
}

impl HotPixel {
    const TOLERANCE: f64 = 0.5;

    /// JTS `HotPixel.intersectsScaled`: whether the segment `p0`-`p1` (in scaled grid coordinates) intersects this
    /// pixel.
    fn intersects(&self, p0: (f64, f64), p1: (f64, f64)) -> bool {
        // determine oriented segment pointing in positive X direction
        let (p, q) = if p0.0 > p1.0 { (p1, p0) } else { (p0, p1) };
        let (hx, hy) = (self.x as f64, self.y as f64);
        let maxx = hx + Self::TOLERANCE;
        if p.0.min(q.0) >= maxx {
            return false;
        }
        let minx = hx - Self::TOLERANCE;
        if p.0.max(q.0) < minx {
            return false;
        }
        let maxy = hy + Self::TOLERANCE;
        if p.1.min(q.1) >= maxy {
            return false;
        }
        let miny = hy - Self::TOLERANCE;
        if p.1.max(q.1) < miny {
            return false;
        }
        // vertical or horizontal segments must now intersect the segment interior or Left or Bottom sides
        if p.0 == q.0 || p.1 == q.1 {
            return true;
        }
        let orient_ul = orientation_index(p, q, (minx, maxy));
        if orient_ul == 0 {
            // upward segment does not intersect pixel interior
            return p.1 >= q.1;
        }
        let orient_ur = orientation_index(p, q, (maxx, maxy));
        if orient_ur == 0 {
            // downward segment does not intersect pixel interior
            return p.1 <= q.1;
        }
        // check crossing Top side
        if orient_ul != orient_ur {
            return true;
        }
        let orient_ll = orientation_index(p, q, (minx, miny));
        if orient_ll == 0 {
            // segment crossed LL corner, which is the only one in pixel interior
            return true;
        }
        // check crossing Left side
        if orient_ll != orient_ul {
            return true;
        }
        let orient_lr = orientation_index(p, q, (maxx, miny));
        if orient_lr == 0 {
            // upward segment does not intersect pixel interior
            return p.1 >= q.1;
        }
        // check crossing Bottom side or Right side
        orient_ll != orient_lr || orient_lr != orient_ur
    }
}

/// Rounds polygon rings to `grid` like JTS's `SnapRoundingNoder`: every vertex becomes a hot pixel and each original
/// segment is noded at every hot pixel it passes through (other than the pixels of its own endpoints). This lets
/// slivers thinner than a grid cell collapse (and be removed by the overlay) the way they do in JTS instead of
/// surviving as tiny triangles.
///
/// TODO(port): JTS also adds hot pixels for segment intersections (and vertices within a tiny tolerance of other
/// segments); here those are only rounded by the integer overlay.
fn snap_rounded_contours(polys: &[&Polygon<f64>], grid: Grid) -> Vec<Contour> {
    let rings: Vec<(&[Coord<f64>], Option<bool>)> = polys
        .iter()
        .filter(|p| !p.exterior().0.is_empty())
        .flat_map(|p| {
            std::iter::once((p.exterior().0.as_slice(), Some(true)))
                .chain(p.interiors().iter().map(|h| (h.0.as_slice(), Some(false))))
        })
        .collect();
    snap_round_rings(&rings, &[], grid)
}

/// Snap-rounds `rings` to `grid` (see [`snap_rounded_contours`]) with `extra_pixels` (in grid units) as additional
/// hot pixels. Each ring is oriented counter-clockwise (`Some(true)`), clockwise (`Some(false)`) or kept as is
/// (`None`).
fn snap_round_rings(rings: &[(&[Coord<f64>], Option<bool>)], extra_pixels: &[(i64, i64)], grid: Grid) -> Vec<Contour> {
    let mut pixels: Vec<(i64, i64)> = rings.iter().flat_map(|(r, _)| r.iter().map(|c| grid.to_int(*c))).collect();
    pixels.extend_from_slice(extra_pixels);
    pixels.sort_unstable();
    pixels.dedup();
    let index = HotPixelIndex::new(&pixels);
    let scaled = |c: &Coord<f64>| (c.x * grid.scale, c.y * grid.scale);
    let mut out = Vec::with_capacity(rings.len());
    let mut nodes: Vec<(f64, (i64, i64))> = Vec::new();
    for &(ring, orientation) in rings {
        let mut contour: Contour = Vec::with_capacity(ring.len());
        for w in ring.windows(2) {
            let (a, b) = (grid.to_int(w[0]), grid.to_int(w[1]));
            if contour.last() != Some(&a) {
                contour.push(a);
            }
            if a == b {
                continue;
            }
            let (p0, p1) = (scaled(&w[0]), scaled(&w[1]));
            let min = (p0.0.min(p1.0) - HotPixel::TOLERANCE, p0.1.min(p1.1) - HotPixel::TOLERANCE);
            let max = (p0.0.max(p1.0) + HotPixel::TOLERANCE, p0.1.max(p1.1) + HotPixel::TOLERANCE);
            nodes.clear();
            let (dx, dy) = (p1.0 - p0.0, p1.1 - p0.1);
            index.for_each_in(min, max, |hp| {
                let key = (hp.x, hp.y);
                if key != a && key != b && hp.intersects(p0, p1) {
                    let t = (hp.x as f64 - p0.0) * dx + (hp.y as f64 - p0.1) * dy;
                    nodes.push((t, key));
                }
            });
            // ties (pixels at the same distance along the segment) are ordered by pixel so the result does not depend
            // on the index's iteration order
            nodes.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.cmp(&y.1)));
            for &(_, key) in &nodes {
                if contour.last() != Some(&key) {
                    contour.push(key);
                }
            }
        }
        if contour.len() > 1 && contour.first() == contour.last() {
            contour.pop();
        }
        out.push(match orientation {
            Some(ccw) => oriented(contour, ccw),
            None => contour,
        });
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

/// Nodes a set of closed curves (which may self-intersect and overlap) and returns the region where their winding
/// number is positive, i.e. counter-clockwise curves add area and clockwise curves remove it. This is how JTS's
/// `BufferBuilder` turns raw offset curves into the buffer polygon (it keeps edges with depth >= 1 on one side and
/// <= 0 on the other).
pub fn positive_winding_region(curves: &[Vec<Coord<f64>>]) -> GeometryResult<Vec<Polygon<f64>>> {
    let mut max = 0f64;
    for c in curves.iter().flatten() {
        max = max.max(c.x.abs()).max(c.y.abs());
    }
    let grid = exact_grid(max);
    let mut subject = Vec::with_capacity(curves.len());
    for curve in curves {
        let mut contour: Contour = Vec::with_capacity(curve.len());
        for c in curve {
            let p = grid.to_int(*c);
            if contour.last() != Some(&p) {
                contour.push(p);
            }
        }
        if contour.len() > 1 && contour.first() == contour.last() {
            contour.pop();
        }
        subject.push(contour);
    }
    let contours = Contours { subject, clip: vec![] };
    let mut result = overlay(&contours, grid, max * grid.scale, OverlayRule::Subject, FillRule::Positive, true)?;
    // JTS's raw curves have the interior on their right (like the output shells), the reverse of `curves`
    let reversed: Vec<Vec<Coord<f64>>> = curves.iter().map(|c| c.iter().rev().copied().collect()).collect();
    InputEdges::new(reversed.iter().map(|c| c.as_slice()), max).match_jts(&mut result);
    Ok(result)
}

type SegmentTree = rstar::RTree<rstar::primitives::GeomWithData<rstar::primitives::Rectangle<[f64; 2]>, (u32, u32)>>;

/// Position of an output edge along the input rings: `(ring, segment, parameter along the segment)`.
type EdgePosition = (u32, u32, f64);

#[inline]
fn position_lt(a: EdgePosition, b: EdgePosition) -> bool {
    (a.0, a.1) < (b.0, b.1) || ((a.0, a.1) == (b.0, b.1) && a.2 < b.2)
}

#[inline]
fn coord_key(c: &Coord<f64>) -> (u64, u64) {
    (c.x.to_bits(), c.y.to_bits())
}

/// [`positive_winding_region`] with JTS's fixed precision noding (`ScaledNoder(SnapRoundingNoder)` at `scale`):
/// every curve vertex and every intersection between curve segments is a hot pixel on the `1 / scale` grid, curve
/// segments are noded at the hot pixels they pass through, and the output is on the grid. The curves' vertices
/// should already be rounded to the grid.
pub fn positive_winding_region_snap_rounded(
    curves: &[Vec<Coord<f64>>],
    intersections: &[Coord<f64>],
    scale: f64,
) -> GeometryResult<Vec<Polygon<f64>>> {
    let grid = Grid { scale, round: true };
    let mut max = 0f64;
    for c in curves.iter().flatten() {
        max = max.max(c.x.abs()).max(c.y.abs());
    }
    let extra: Vec<(i64, i64)> = intersections.iter().map(|c| grid.to_int(*c)).collect();
    let rings: Vec<(&[Coord<f64>], Option<bool>)> = curves.iter().map(|c| (c.as_slice(), None)).collect();
    let contours = Contours { subject: snap_round_rings(&rings, &extra, grid), clip: vec![] };
    overlay(&contours, grid, max * scale + 1.0, OverlayRule::Subject, FillRule::Positive, true)
}

/// The edges of the input rings of an overlay, used to make the output vertices and ring start points match what
/// JTS's overlay / buffer builders produce for the same input.
struct InputEdges<'a> {
    rings: Vec<&'a [Coord<f64>]>,
    /// input vertex -> index in `occurrences` of its last occurrence
    vertices: hashbrown::HashMap<(u64, u64), u32>,
    /// (ring, index) of each vertex occurrence, and the index of the previous occurrence of the same vertex
    occurrences: Vec<((u32, u32), u32)>,
    tree: std::cell::OnceCell<SegmentTree>,
    tolerance: f64,
}

impl<'a> InputEdges<'a> {
    fn new(rings: impl Iterator<Item = &'a [Coord<f64>]>, max_abs: f64) -> InputEdges<'a> {
        let rings: Vec<&[Coord<f64>]> = rings.collect();
        let n: usize = rings.iter().map(|r| r.len()).sum();
        let mut vertices: hashbrown::HashMap<(u64, u64), u32> = hashbrown::HashMap::with_capacity(n);
        let mut occurrences = Vec::with_capacity(n);
        for (r, ring) in rings.iter().enumerate() {
            // the closing vertex of a closed ring is its first vertex (and starts no segment)
            let len = if ring.len() > 1 && ring[0] == ring[ring.len() - 1] { ring.len() - 1 } else { ring.len() };
            for (s, c) in ring[..len].iter().enumerate() {
                let index = occurrences.len() as u32;
                let previous = vertices.insert(coord_key(c), index).unwrap_or(u32::MAX);
                occurrences.push(((r as u32, s as u32), previous));
            }
        }
        InputEdges {
            rings,
            vertices,
            occurrences,
            tree: std::cell::OnceCell::new(),
            tolerance: max_abs * 2f64.powi(-40),
        }
    }

    /// The segment `s` of ring `r` (the closing segment wraps around for unclosed rings), or `None` past the end.
    #[inline]
    fn segment(&self, r: u32, s: u32) -> Option<(Coord<f64>, Coord<f64>)> {
        let ring = self.rings[r as usize];
        let s = s as usize;
        if s + 1 < ring.len() {
            Some((ring[s], ring[s + 1]))
        } else if s + 1 == ring.len() && ring.len() > 1 && ring[0] != ring[s] {
            Some((ring[s], ring[0]))
        } else {
            None
        }
    }

    fn tree(&self) -> &SegmentTree {
        use rstar::primitives::{GeomWithData, Rectangle};
        self.tree.get_or_init(|| {
            let mut items = Vec::new();
            for (r, ring) in self.rings.iter().enumerate() {
                for s in 0..ring.len() {
                    if let Some((a, b)) = self.segment(r as u32, s as u32)
                        && a != b
                    {
                        let rect = Rectangle::from_corners([a.x, a.y], [b.x, b.y]);
                        items.push(GeomWithData::new(rect, (r as u32, s as u32)));
                    }
                }
            }
            rstar::RTree::bulk_load(items)
        })
    }

    /// Input segments within the tolerance of `c`.
    fn segments_near(&self, c: Coord<f64>, out: &mut Vec<(u32, u32)>) {
        use crate::geo::cg_algorithms::point_to_segment;
        let t = self.tolerance;
        let envelope = rstar::AABB::from_corners([c.x - t, c.y - t], [c.x + t, c.y + t]);
        out.clear();
        for item in self.tree().locate_in_envelope_intersecting(envelope) {
            let (r, s) = item.data;
            let (a, b) = self.segment(r, s).expect("indexed segment");
            if point_to_segment(c, a, b) <= t {
                out.push((r, s));
            }
        }
    }

    /// Replaces the output vertices that are not input vertices (the intersection points between input edges,
    /// which `i_overlay` computes on its integer grid with a few ulps of error) with the intersection point JTS
    /// computes for the same pair of input segments (`RobustLineIntersector`, double-double arithmetic), so that
    /// results that land exactly on a rounding boundary later (e.g. half a tile pixel unit) round the same way as in
    /// Java.
    fn refine_intersection_vertices(&self, polys: &mut [Polygon<f64>]) {
        use crate::geo::cg_algorithms::{LineIntersector, distance};
        let is_new = |c: &Coord<f64>| !self.vertices.contains_key(&coord_key(c));
        let mut li = LineIntersector::new();
        let mut candidates: Vec<(u32, u32)> = Vec::new();
        let mut refine = |c: &mut Coord<f64>| {
            self.segments_near(*c, &mut candidates);
            let mut best: Option<(f64, Coord<f64>)> = None;
            for (n, &i) in candidates.iter().enumerate() {
                for &j in &candidates[n + 1..] {
                    let (p1, p2) = self.segment(i.0, i.1).expect("indexed segment");
                    let (q1, q2) = self.segment(j.0, j.1).expect("indexed segment");
                    li.compute_intersection(p1, p2, q1, q2);
                    for k in 0..li.intersection_num() as usize {
                        let p = li.intersection(k);
                        let d = distance(p, *c);
                        if d <= self.tolerance && best.is_none_or(|(bd, _)| d < bd) {
                            best = Some((d, p));
                        }
                    }
                }
            }
            if let Some((_, p)) = best {
                *c = p;
            }
        };
        let fix_ring = |ring: &mut LineString<f64>, refine: &mut dyn FnMut(&mut Coord<f64>)| {
            let n = ring.0.len();
            if n < 2 {
                return;
            }
            let mut changed = false;
            for c in &mut ring.0[..n - 1] {
                if is_new(c) {
                    refine(c);
                    changed = true;
                }
            }
            ring.0[n - 1] = ring.0[0];
            if changed {
                // two output vertices within a few ulps of the same intersection collapse into one
                ring.0.dedup();
            }
        };
        for poly in polys.iter_mut() {
            poly.exterior_mut(|r| fix_ring(r, &mut refine));
            poly.interiors_mut(|rings| rings.iter_mut().for_each(|r| fix_ring(r, &mut refine)));
        }
    }

    /// Position of the output edge `p -> q` along the input rings (the first input segment it follows forward), or
    /// `None` if there is none.
    fn edge_position(&self, p: Coord<f64>, q: Coord<f64>, scratch: &mut Vec<(u32, u32)>) -> Option<EdgePosition> {
        use crate::geo::cg_algorithms::point_to_segment;
        let mut best: Option<EdgePosition> = None;
        let consider = |r: u32, s: u32, best: &mut Option<EdgePosition>| {
            let Some((a, b)) = self.segment(r, s) else { return };
            let (dx, dy) = (b.x - a.x, b.y - a.y);
            if a == b || dx * (q.x - p.x) + dy * (q.y - p.y) <= 0.0 || point_to_segment(q, a, b) > self.tolerance {
                return;
            }
            let key = (r, s, ((p.x - a.x) * dx + (p.y - a.y) * dy) / (dx * dx + dy * dy));
            if best.is_none_or(|b| position_lt(key, b)) {
                *best = Some(key);
            }
        };
        if let Some(&last) = self.vertices.get(&coord_key(&p)) {
            let mut index = last;
            while index != u32::MAX {
                let ((r, s), previous) = self.occurrences[index as usize];
                consider(r, s, &mut best);
                index = previous;
            }
        }
        if best.is_none() {
            self.segments_near(p, scratch);
            for &(r, s) in scratch.iter() {
                consider(r, s, &mut best);
            }
        }
        best
    }

    /// Rotates each output ring to start where JTS's overlay graph would start it: at the start of the result edge
    /// that comes first along the input rings (JTS builds each result ring from the first result edge in the order
    /// the noded input edges were added to the graph).
    fn rotate_to_jts_start(&self, polys: &mut [Polygon<f64>]) {
        let mut scratch = Vec::new();
        let mut rotate = |ring: &mut LineString<f64>| {
            let n = ring.0.len();
            if n < 4 {
                return;
            }
            let mut best: Option<(usize, EdgePosition)> = None;
            for i in 0..n - 1 {
                if let Some(key) = self.edge_position(ring.0[i], ring.0[i + 1], &mut scratch)
                    && best.is_none_or(|(_, b)| position_lt(key, b))
                {
                    best = Some((i, key));
                }
            }
            if let Some((start, _)) = best
                && start != 0
            {
                ring.0.pop();
                ring.0.rotate_left(start);
                ring.0.push(ring.0[0]);
            }
        };
        for poly in polys.iter_mut() {
            poly.exterior_mut(&mut rotate);
            poly.interiors_mut(|rings| rings.iter_mut().for_each(&mut rotate));
        }
    }

    /// [`Self::refine_intersection_vertices`] then [`Self::rotate_to_jts_start`].
    fn match_jts(&self, polys: &mut [Polygon<f64>]) {
        self.refine_intersection_vertices(polys);
        self.rotate_to_jts_start(polys);
    }
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
        let contours = Contours { subject: snap_rounded_contours(&polys, grid), clip: vec![] };
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

/// Union of two (each valid) polygonal geometries like JTS's classic `OverlayOp.overlayOp(a, b, UNION)`: the
/// intersection points between their edges are the ones JTS computes for the same pair of input segments.
pub fn union_pair(a: &[Polygon<f64>], b: &[Polygon<f64>]) -> GeometryResult<Vec<Polygon<f64>>> {
    let bounds =
        |polys: &[Polygon<f64>]| {
            polys.iter().map(|p| ring_bounds(p.exterior())).fold(
                (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY),
                |(a, b, c, d), (e, f, g, h)| (a.min(e), b.min(f), c.max(g), d.max(h)),
            )
        };
    let (ea, eb) = (bounds(a), bounds(b));
    if ea.0 > eb.2 || eb.0 > ea.2 || ea.1 > eb.3 || eb.1 > ea.3 {
        // disjoint envelopes: JTS's result rings are the input rings (each one an unsplit result edge)
        return Ok(a.iter().chain(b).cloned().collect());
    }
    let pa: Vec<&Polygon<f64>> = a.iter().collect();
    let pb: Vec<&Polygon<f64>> = b.iter().collect();
    let max = max_abs_coord(pa.iter().chain(pb.iter()).copied());
    let grid = exact_grid(max);
    let contours = Contours { subject: positive_contours(&pa, grid), clip: positive_contours(&pb, grid) };
    let mut result = overlay(&contours, grid, max * grid.scale, OverlayRule::Union, FillRule::Positive, true)?;
    let rings =
        a.iter().chain(b).flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors())).map(|r| r.0.as_slice());
    InputEdges::new(rings, max).match_jts(&mut result);
    Ok(result)
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
    let mut result = overlay(&contours, grid, max * grid.scale, OverlayRule::Subject, FillRule::NonZero, true)?;
    let rings = polys.iter().flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors())).map(|r| r.0.as_slice());
    InputEdges::new(rings, max).refine_intersection_vertices(&mut result);
    Ok(to_polygonal(result))
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
    fn rebind_misplaced_holes_moves_hole_to_containing_shell() {
        let big = rectangle(0.0, 10.0);
        let small = rectangle(20.0, 21.0);
        let hole = LineString::from(rectangle_coord_list(2.0, 3.0));
        let island = rectangle(4.0, 6.0);
        let island_hole = LineString::from(rectangle_coord_list(4.5, 5.0));
        let lake = LineString::from(rectangle_coord_list(3.5, 6.5));
        // holes attached to the wrong shells
        let input = vec![
            Polygon::new(big.exterior().clone(), vec![]),
            Polygon::new(small.exterior().clone(), vec![hole.clone(), lake.clone(), island_hole.clone()]),
            Polygon::new(island.exterior().clone(), vec![]),
        ];
        let result = rebind_misplaced_holes(input);
        assert_eq!(vec![hole, lake], result[0].interiors());
        assert!(result[1].interiors().is_empty());
        assert_eq!(vec![island_hole], result[2].interiors());
        assert!(is_valid(&MultiPolygon(result).into()));
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

    #[test]
    fn coordinates_just_above_a_power_of_two_fit_the_grid() {
        let max = 256.0 + 256.0 * f64::EPSILON;
        let grid = exact_grid(max);
        assert!(max * grid.scale <= I64_LIMIT);
        let p: Geometry<f64> = rectangle_xy(0.0, 0.0, max, 10.0).into();
        assert!((buffer_zero(&p).unwrap().unsigned_area() - max * 10.0).abs() < 1e-9);
    }
}

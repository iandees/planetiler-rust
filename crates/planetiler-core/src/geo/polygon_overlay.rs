//! Polygon repair, precision reduction and boolean operations built on `i_overlay`'s integer overlay engine.
//!
//! These replace the JTS operations planetiler relies on:
//!
//! - [`buffer_zero`]: JTS `geom.buffer(0)`. Each polygon's shell is oriented to have positive signed area and its
//!   holes negative, then everything is resolved with a *positive* winding fill rule. Like JTS, a self-intersecting
//!   "bow-tie" keeps only the lobes that wind the same way as the overall ring.
//! - [`geometry_fixer`]: JTS `GeometryFixer.fix`. Rings are resolved with a *non-zero* fill (both lobes of a bow-tie
//!   are kept), holes are subtracted from their shell and the resulting polygons are unioned.
//! - [`reduce_precision`]: JTS `GeometryPrecisionReducer.reduce`. Polygon rings are snap-rounded to the precision
//!   grid with a port of JTS's `SnapRoundingNoder` ([`crate::geo::snap_rounding`]) and the enclosed region is
//!   rebuilt, like JTS's OverlayNG; lines and points are rounded pointwise. The output is always valid, so unlike JTS
//!   this never fails.
//! - [`union`], [`difference`], [`intersection`]: polygon boolean operations.
//!
//! Output polygons follow the JTS overlay convention: shells clockwise and holes counter-clockwise (in the usual
//! y-up sense used by `geo::Winding`). Output is OGC-valid (holes that touch the shell at a point are separate holes,
//! not self-touching shells).

use geo::Area;
use geo_types::{
    Coord, Geometry, GeometryCollection, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon,
};
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
use crate::geo::snap_rounding;

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

/// Equivalent of JTS `geom.buffer(0)` for polygonal geometries: resolves self-intersections and overlaps. Points and
/// lines produce an empty polygon.
pub fn buffer_zero(geom: &Geometry<f64>) -> GeometryResult<Geometry<f64>> {
    let polys = polygons_of(geom);
    let grid = exact_grid(max_abs_coord(polys.iter().copied()));
    let contours = Contours { subject: positive_contours(&polys, grid), clip: vec![] };
    let max_int = max_abs_coord(polys.iter().copied()) * grid.scale;
    let mut result = overlay(&contours, grid, max_int, OverlayRule::Subject, FillRule::Positive, true)?;
    restore_vertices_on_edges(&polys, &mut result);
    Ok(to_polygonal(result))
}

/// Inserts every input vertex that lies exactly on the interior of an output edge into that edge.
///
/// JTS's noding splits an input segment at every input vertex that touches it (e.g. the tip of a spike folded back
/// along another edge), so those vertices are nodes of its graph and stay in the output rings as collinear vertices,
/// while `i_overlay` merges the collinear output segments around them.
fn restore_vertices_on_edges(input: &[&Polygon<f64>], output: &mut [Polygon<f64>]) {
    use crate::geo::cg_algorithms::orientation_index;
    // only the input vertices that are not already output vertices can be missing
    let mut present: hashbrown::HashSet<(u64, u64)> = hashbrown::HashSet::new();
    for p in output.iter() {
        for r in std::iter::once(p.exterior()).chain(p.interiors()) {
            present.extend(r.0.iter().map(coord_key));
        }
    }
    let mut by_x: Vec<Coord<f64>> = input
        .iter()
        .flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors()))
        .flat_map(|r| r.0.iter().copied())
        .filter(|c| c.x.is_finite() && c.y.is_finite() && !present.contains(&coord_key(c)))
        .collect();
    if by_x.is_empty() {
        return;
    }
    by_x.sort_unstable_by(|a, b| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)));
    by_x.dedup();
    let mut by_y = by_x.clone();
    by_y.sort_unstable_by(|a, b| a.y.total_cmp(&b.y).then(a.x.total_cmp(&b.x)));
    let mut found: Vec<(f64, Coord<f64>)> = Vec::new();
    let mut fix_ring = |ring: &mut LineString<f64>| {
        let n = ring.0.len();
        if n < 2 {
            return;
        }
        let mut out: Option<Vec<Coord<f64>>> = None;
        for i in 0..n - 1 {
            let (a, b) = (ring.0[i], ring.0[i + 1]);
            let (min_x, max_x, min_y, max_y) = (a.x.min(b.x), a.x.max(b.x), a.y.min(b.y), a.y.max(b.y));
            let x_range = by_x.partition_point(|c| c.x < min_x)..by_x.partition_point(|c| c.x <= max_x);
            let y_range = by_y.partition_point(|c| c.y < min_y)..by_y.partition_point(|c| c.y <= max_y);
            let candidates = if x_range.len() <= y_range.len() { &by_x[x_range] } else { &by_y[y_range] };
            found.clear();
            for &v in candidates {
                if v != a
                    && v != b
                    && min_x <= v.x
                    && v.x <= max_x
                    && min_y <= v.y
                    && v.y <= max_y
                    && orientation_index(a, b, v) == 0
                {
                    let (dx, dy) = (v.x - a.x, v.y - a.y);
                    found.push((dx * dx + dy * dy, v));
                }
            }
            if found.is_empty() && out.is_none() {
                continue;
            }
            let out = out.get_or_insert_with(|| ring.0[..=i].to_vec());
            found.sort_by(|x, y| x.0.total_cmp(&y.0));
            out.extend(found.iter().map(|&(_, v)| v));
            out.push(b);
        }
        if let Some(out) = out {
            ring.0 = out;
        }
    };
    for poly in output {
        poly.exterior_mut(&mut fix_ring);
        poly.interiors_mut(|rings| rings.iter_mut().for_each(&mut fix_ring));
    }
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

/// [`positive_winding_region`] of a single closed `curve` that is known to be simple (no self-intersections or
/// touches, no repeated consecutive vertices, e.g. checked by `buffer::noding::is_simple_closed_curve`), without
/// running the overlay: the region is the curve's interior if it is counter-clockwise and empty if it is clockwise.
///
/// Returns `None` (use [`positive_winding_region`]) unless every vertex is exactly representable on the overlay's
/// integer grid, so the result is exactly what the overlay would produce: the same ring, clockwise, with all
/// vertices (collinear ones are preserved), rotated to JTS's start point by the same [`InputEdges`] matching.
pub fn simple_curve_positive_winding_region(curve: &[Coord<f64>]) -> Option<Vec<Polygon<f64>>> {
    let mut max = 0f64;
    for c in curve {
        max = max.max(c.x.abs()).max(c.y.abs());
    }
    let grid = exact_grid(max);
    if max.is_nan() || max * grid.scale > I64_LIMIT {
        return None;
    }
    let n = curve.len();
    if n < 4 || curve[0] != curve[n - 1] {
        return None;
    }
    let mut contour: Contour = Vec::with_capacity(n - 1);
    for c in &curve[..n - 1] {
        let p = grid.to_int(*c);
        let back = grid.to_float(p.0, p.1);
        // exact (also excludes -0.0, which the overlay would output as 0.0)
        if back.x.to_bits() != c.x.to_bits() || back.y.to_bits() != c.y.to_bits() || contour.last() == Some(&p) {
            return None;
        }
        contour.push(p);
    }
    if contour.len() < 3 || contour.first() == contour.last() {
        return None;
    }
    let area = signed_area2(&contour);
    if area < 0 {
        return Some(vec![]);
    } else if area == 0 {
        return None;
    }
    let ring: Vec<Coord<f64>> = curve.iter().rev().copied().collect();
    // closed: the first vertex is repeated at the end (`curve[0] == curve[n - 1]`)
    debug_assert_eq!(ring[0], ring[n - 1]);
    let reversed: Vec<Coord<f64>> = ring.clone();
    let mut result = vec![Polygon::new(LineString(ring), vec![])];
    InputEdges::new(std::iter::once(reversed.as_slice()), max).match_jts(&mut result);
    Some(result)
}

type SegmentTree = crate::geo::segment_grid::SegmentGrid<(u32, u32)>;
type RTreeIndex = rstar::RTree<rstar::primitives::GeomWithData<rstar::primitives::Rectangle<[f64; 2]>, (u32, u32)>>;

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

/// [`positive_winding_region`] with JTS's fixed precision noding of buffer curves,
/// `ScaledNoder(SnapRoundingNoder(PrecisionModel(1)), scale)`: the curves are scaled by `scale` and rounded to
/// integers, snap-rounded on the unit grid with [`snap_rounding::snap_round`] and scaled back, so the output is on
/// the `1 / scale` grid.
pub fn positive_winding_region_scaled_snap_rounded(
    curves: &[Vec<Coord<f64>>],
    scale: f64,
) -> GeometryResult<Vec<Polygon<f64>>> {
    let mut max = 0f64;
    let mut strings: Vec<Vec<Coord<f64>>> = Vec::with_capacity(curves.len());
    for curve in curves {
        // ScaledNoder.scale: round to the scaled grid and remove repeated points
        let mut pts: Vec<Coord<f64>> = Vec::with_capacity(curve.len());
        for c in curve {
            let p = Coord { x: java_round(c.x * scale), y: java_round(c.y * scale) };
            max = max.max(p.x.abs()).max(p.y.abs());
            if pts.last() != Some(&p) {
                pts.push(p);
            }
        }
        if pts.len() >= 2 {
            strings.push(pts);
        }
    }
    let refs: Vec<&[Coord<f64>]> = strings.iter().map(|s| s.as_slice()).collect();
    let mut subject = Vec::with_capacity(strings.len());
    for mut contour in snap_rounding::snap_round(&refs, 1.0).into_iter().flatten() {
        if contour.len() > 1 && contour.first() == contour.last() {
            contour.pop();
        }
        subject.push(contour);
    }
    let contours = Contours { subject, clip: vec![] };
    overlay(&contours, Grid { scale, round: true }, max + 1.0, OverlayRule::Subject, FillRule::Positive, true)
}

/// The edges of the input rings of an overlay, used to make the output vertices and ring start points match what
/// JTS's overlay / buffer builders produce for the same input.
struct InputEdges<'a> {
    rings: Vec<&'a [Coord<f64>]>,
    /// input vertex -> index in `occurrences` of its last occurrence
    vertices: hashbrown::HashMap<(u64, u64), u32>,
    /// (ring, index) of each vertex occurrence, and the index of the previous occurrence of the same vertex
    occurrences: Vec<((u32, u32), u32)>,
    grid: std::cell::OnceCell<SegmentTree>,
    rtree: std::cell::OnceCell<RTreeIndex>,
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
            grid: std::cell::OnceCell::new(),
            rtree: std::cell::OnceCell::new(),
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

    fn grid(&self) -> &SegmentTree {
        self.grid.get_or_init(|| {
            let items = self.rings.iter().enumerate().flat_map(|(r, ring)| {
                (0..ring.len()).filter_map(move |s| {
                    let (a, b) = self.segment(r as u32, s as u32)?;
                    (a != b).then(|| ([a.x.min(b.x), a.y.min(b.y), a.x.max(b.x), a.y.max(b.y)], (r as u32, s as u32)))
                })
            });
            SegmentTree::new(items)
        })
    }

    /// Input segments within the tolerance of `c`, in input order.
    fn segments_near(&self, c: Coord<f64>, out: &mut Vec<(u32, u32)>) {
        use crate::geo::cg_algorithms::point_to_segment;
        let t = self.tolerance;
        out.clear();
        self.grid().query([c.x - t, c.y - t], [c.x + t, c.y + t], out);
        out.retain(|&(r, s)| {
            let (a, b) = self.segment(r, s).expect("indexed segment");
            point_to_segment(c, a, b) <= t
        });
    }

    /// [`Self::segments_near`] in the order of an `rstar` R-tree query.
    fn segments_near_rtree_order(&self, c: Coord<f64>, out: &mut Vec<(u32, u32)>) {
        use crate::geo::cg_algorithms::point_to_segment;
        use rstar::primitives::{GeomWithData, Rectangle};
        let tree = self.rtree.get_or_init(|| {
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
        });
        let t = self.tolerance;
        let envelope = rstar::AABB::from_corners([c.x - t, c.y - t], [c.x + t, c.y + t]);
        out.clear();
        for item in tree.locate_in_envelope_intersecting(envelope) {
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
    ///
    /// The closest intersection point of two input segments near the vertex wins (the first one in candidate order
    /// on ties). Candidates come from the grid index; when the winner could depend on their order (another point at
    /// the same distance, or a pair whose intersection depends on which segment is first) they are taken in the
    /// order of an R-tree query instead, which is what this used before the grid index (so output is unchanged).
    fn refine_intersection_vertices(&self, polys: &mut [Polygon<f64>]) {
        use crate::geo::cg_algorithms::{LineIntersector, distance};
        let is_new = |c: &Coord<f64>| !self.vertices.contains_key(&coord_key(c));
        let tolerance = self.tolerance;
        let mut li = LineIntersector::new();
        let mut candidates: Vec<(u32, u32)> = Vec::new();
        let mut points: Vec<(f64, Coord<f64>)> = Vec::new();
        let mut refine = |c: &mut Coord<f64>| {
            self.segments_near(*c, &mut candidates);
            // every intersection point within the tolerance, or None if a pair's intersection depends on the order
            // of the two segments
            points.clear();
            let mut order_independent = true;
            'pairs: for (n, &i) in candidates.iter().enumerate() {
                for &j in &candidates[n + 1..] {
                    let (p1, p2) = self.segment(i.0, i.1).expect("indexed segment");
                    let (q1, q2) = self.segment(j.0, j.1).expect("indexed segment");
                    li.compute_intersection(q1, q2, p1, p2);
                    let reverse = (li.intersection_num(), li.intersection(0), li.intersection(1));
                    li.compute_intersection(p1, p2, q1, q2);
                    let num = li.intersection_num();
                    let (a, b) = (li.intersection(0), li.intersection(1));
                    let same = reverse.0 == num
                        && match num {
                            0 => true,
                            1 => reverse.1 == a,
                            _ => (reverse.1 == a && reverse.2 == b) || (reverse.1 == b && reverse.2 == a),
                        };
                    if !same {
                        order_independent = false;
                        break 'pairs;
                    }
                    for k in 0..num as usize {
                        let p = li.intersection(k);
                        let d = distance(p, *c);
                        if d <= tolerance {
                            points.push((d, p));
                        }
                    }
                }
            }
            let mut best: Option<(f64, Coord<f64>)> = None;
            if order_independent {
                for &(d, p) in &points {
                    if best.is_none_or(|(bd, _)| d < bd) {
                        best = Some((d, p));
                    }
                }
                if let Some((bd, bp)) = best
                    && points.iter().any(|&(d, p)| d == bd && p != bp)
                {
                    order_independent = false;
                }
            }
            if !order_independent {
                self.segments_near_rtree_order(*c, &mut candidates);
                best = None;
                for (n, &i) in candidates.iter().enumerate() {
                    for &j in &candidates[n + 1..] {
                        let (p1, p2) = self.segment(i.0, i.1).expect("indexed segment");
                        let (q1, q2) = self.segment(j.0, j.1).expect("indexed segment");
                        li.compute_intersection(p1, p2, q1, q2);
                        for k in 0..li.intersection_num() as usize {
                            let p = li.intersection(k);
                            let d = distance(p, *c);
                            if d <= tolerance && best.is_none_or(|(bd, _)| d < bd) {
                                best = Some((d, p));
                            }
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

/// Equivalent of JTS `GeometryPrecisionReducer.reduce(geom, precisionModel)` (`PrecisionReducerTransformer`, which
/// removes collapsed components):
///
/// - Each polygon / multipolygon (also inside a geometry collection) is reduced like JTS's
///   `OverlayNG(geom, precisionModel)` area-only union: its rings are snap-rounded to the grid with
///   [`snap_rounding::snap_round`] (JTS `SnapRoundingNoder`: hot pixels at every vertex and every intersection or
///   near-vertex, every segment snapped to the hot pixels it passes through) and the result is the region they enclose
///   (each ring counted with the side JTS's depth delta gives it: shells enclose area, holes remove it, whatever their
///   orientation). Collapsed parts are removed. The output is always valid, so unlike JTS this never fails.
/// - Points, lines and multipoints are rounded pointwise like JTS: repeated points are removed from lines and lines
///   that collapse to fewer than 2 points are removed (a single line becomes `LINESTRING EMPTY`), a multipoint or
///   multilinestring with one remaining part becomes that part.
pub fn reduce_precision(geom: &Geometry<f64>, pm: &PrecisionModel) -> GeometryResult<Geometry<f64>> {
    let scale = match pm {
        PrecisionModel::Floating => return Ok(geom.clone()),
        PrecisionModel::Fixed { scale } => *scale,
    };
    let reduce_line = |ls: &LineString<f64>| -> LineString<f64> {
        let mut coords: Vec<Coord<f64>> = Vec::with_capacity(ls.0.len());
        for c in &ls.0 {
            let c = pm.make_precise_coord(*c);
            if coords.last() != Some(&c) {
                coords.push(c);
            }
        }
        LineString(if coords.len() >= 2 { coords } else { vec![] })
    };
    Ok(match geom {
        Geometry::Point(p) => Geometry::Point(Point(pm.make_precise_coord(p.0))),
        Geometry::MultiPoint(mp) => {
            let mut points: Vec<Point<f64>> = mp.0.iter().map(|p| Point(pm.make_precise_coord(p.0))).collect();
            if points.len() == 1 {
                Geometry::Point(points.pop().unwrap())
            } else {
                Geometry::MultiPoint(MultiPoint(points))
            }
        }
        Geometry::Line(l) => Geometry::LineString(reduce_line(&(*l).into())),
        Geometry::LineString(ls) => Geometry::LineString(reduce_line(ls)),
        Geometry::MultiLineString(mls) => {
            let mut lines: Vec<LineString<f64>> = mls.0.iter().map(reduce_line).filter(|l| !l.0.is_empty()).collect();
            if lines.len() == 1 {
                Geometry::LineString(lines.pop().unwrap())
            } else {
                Geometry::MultiLineString(MultiLineString(lines))
            }
        }
        Geometry::Polygon(p) => to_polygonal(reduce_area(&[p], scale)?),
        Geometry::MultiPolygon(mp) => {
            let polys: Vec<&Polygon<f64>> = mp.0.iter().collect();
            // an empty OverlayNG area result is an empty polygon
            to_polygonal(reduce_area(&polys, scale)?)
        }
        Geometry::Rect(r) => to_polygonal(reduce_area(&[&r.to_polygon()], scale)?),
        Geometry::Triangle(t) => to_polygonal(reduce_area(&[&t.to_polygon()], scale)?),
        Geometry::GeometryCollection(gc) => {
            let mut parts = Vec::with_capacity(gc.0.len());
            for g in &gc.0 {
                let reduced = reduce_precision(g, pm)?;
                if !is_empty(&reduced) {
                    parts.push(reduced);
                }
            }
            Geometry::GeometryCollection(GeometryCollection(parts))
        }
    })
}

fn is_empty(geom: &Geometry<f64>) -> bool {
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

/// JTS `OverlayNG(geom, pm)` with `setAreaResultOnly(true)` (`overlayng.PrecisionReducer.reducePrecision`) for the
/// polygons of one polygonal geometry.
fn reduce_area(polys: &[&Polygon<f64>], scale: f64) -> GeometryResult<Vec<Polygon<f64>>> {
    // EdgeNodingBuilder.addPolygon: the shell then the holes, repeated points removed, collapsed rings dropped
    let mut strings: Vec<Vec<Coord<f64>>> = Vec::new();
    let mut reverse: Vec<bool> = Vec::new();
    let mut max = 0f64;
    for poly in polys {
        for (i, ring) in std::iter::once(poly.exterior()).chain(poly.interiors()).enumerate() {
            let mut pts: Vec<Coord<f64>> = Vec::with_capacity(ring.0.len());
            for c in &ring.0 {
                if pts.last() != Some(c) {
                    pts.push(*c);
                }
                max = max.max(c.x.abs()).max(c.y.abs());
            }
            if pts.len() < 2 {
                continue;
            }
            // EdgeNodingBuilder.computeDepthDelta: the interior is on the right of a clockwise shell or a
            // counter-clockwise hole (orientation of the input ring); the positive fill rule wants it on the left
            let ccw = crate::geo::cg_algorithms::is_ccw(&ring.0);
            reverse.push(if i == 0 { !ccw } else { ccw });
            strings.push(pts);
        }
    }
    if strings.is_empty() {
        return Ok(vec![]);
    }
    let refs: Vec<&[Coord<f64>]> = strings.iter().map(|s| s.as_slice()).collect();
    let mut subject = Vec::with_capacity(strings.len());
    for (snapped, reverse) in snap_rounding::snap_round(&refs, scale).into_iter().zip(reverse) {
        let Some(mut contour) = snapped else { continue };
        if contour.len() > 1 && contour.first() == contour.last() {
            contour.pop();
        }
        if reverse {
            contour.reverse();
        }
        subject.push(contour);
    }
    let grid = Grid { scale, round: true };
    let contours = Contours { subject, clip: vec![] };
    overlay(&contours, grid, max * scale + 1.0, OverlayRule::Subject, FillRule::Positive, true)
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

    /// A case generated by `bench/java/SnapRoundCases.java`: the input and Java's `GeoUtils.snapAndFixPolygon` and
    /// `GeometryPrecisionReducer.reduce` outputs at tile precision (`None` where Java throws).
    struct JavaCase {
        kind: String,
        input: Geometry<f64>,
        snap: Option<Geometry<f64>>,
        reduce: Option<Geometry<f64>>,
    }

    fn read_java_cases(name: &str) -> Vec<JavaCase> {
        use crate::geo::geo_utils::read_wkb;
        let path = if name.starts_with('/') {
            std::path::PathBuf::from(name)
        } else {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/resources/snapround").join(name)
        };
        let data = crate::util::gzip::gunzip(&std::fs::read(path).unwrap()).unwrap();
        let text = String::from_utf8(data).unwrap();
        let parse = |hex: &str| -> Option<Geometry<f64>> {
            (hex != "ERROR").then(|| {
                let bytes: Vec<u8> =
                    (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
                read_wkb(&bytes).unwrap()
            })
        };
        let mut lines = text.lines();
        let mut cases = Vec::new();
        while let Some(line) = lines.next() {
            let mut parts = line.split(' ');
            assert_eq!(Some("CASE"), parts.next());
            let kind = parts.next().unwrap().to_string();
            let input = parse(parts.next().unwrap()).unwrap();
            let snap = lines.next().unwrap().strip_prefix("SNAP ").unwrap();
            let reduce = lines.next().unwrap().strip_prefix("REDUCE ").unwrap();
            cases.push(JavaCase { kind, input, snap: parse(snap), reduce: parse(reduce) });
        }
        cases
    }

    /// Results of [`check_java_cases`]: descriptions of the outputs that differ from Java's (normalized).
    #[derive(Default)]
    struct JavaCaseFailures {
        cases: usize,
        /// `snap_and_fix_polygon` results, and `reduce_precision` results on valid inputs
        failures: Vec<String>,
        /// `reduce_precision` results on invalid inputs, which `GeometryPrecisionReducer` does not support (Java's
        /// `snapAndFixPolygon` fixes invalid input before reducing it): what JTS keeps there depends on how its
        /// overlay labels the inconsistent sides of self-overlapping rings
        invalid_reduce_failures: Vec<String>,
    }

    /// Runs `reduce_precision` and `snap_and_fix_polygon` (tile precision) on every case of `name` (a file in
    /// `tests/resources/snapround`, or an absolute path) and compares them with Java's results.
    fn check_java_cases(name: &str) -> JavaCaseFailures {
        use crate::geo::geo_utils::{TILE_PRECISION, snap_and_fix_polygon};
        use crate::geo::normalize::norm;
        use crate::stats::stats::InMemoryStats;
        let cases = read_java_cases(name);
        let mut result = JavaCaseFailures { cases: cases.len(), ..Default::default() };
        let mut counts: std::collections::BTreeMap<(String, &str), usize> = Default::default();
        for (i, case) in cases.iter().enumerate() {
            if let Some(expected) = &case.reduce {
                let actual = reduce_precision(&case.input, &TILE_PRECISION).unwrap();
                if norm(expected) != norm(&actual) {
                    let valid = is_valid(&case.input);
                    *counts.entry((case.kind.clone(), if valid { "reduce" } else { "reduce invalid" })).or_default() +=
                        1;
                    let text = format!(
                        "case {i} ({}) reduce of {:?}:\n  expected {:?}\n  actual   {:?}",
                        case.kind,
                        case.input,
                        norm(expected),
                        norm(&actual)
                    );
                    if valid { result.failures.push(text) } else { result.invalid_reduce_failures.push(text) }
                }
            }
            if let Some(expected) = &case.snap {
                let actual = snap_and_fix_polygon(&case.input, &InMemoryStats::new(), "test").unwrap();
                if norm(expected) != norm(&actual) {
                    *counts.entry((case.kind.clone(), "snap")).or_default() += 1;
                    result.failures.push(format!(
                        "case {i} ({}) snapAndFixPolygon of {:?}:\n  expected {:?}\n  actual   {:?}",
                        case.kind,
                        case.input,
                        norm(expected),
                        norm(&actual)
                    ));
                }
            }
        }
        eprintln!("{name}: {} cases, differences by kind: {counts:?}", cases.len());
        result
    }

    fn assert_no_failures(failures: &[String]) {
        assert!(failures.is_empty(), "{} failures:\n{}", failures.len(), failures[..failures.len().min(5)].join("\n"));
    }

    /// Synthetic near-touching / self-touching / sliver / spike polygons, invalid bow-ties, lines and collections.
    #[test]
    fn reduce_precision_matches_java_on_synthetic_cases() {
        let result = check_java_cases("synthetic.txt.gz");
        assert!(result.cases > 1000);
        assert_no_failures(&result.failures);
        assert_no_failures(&result.invalid_reduce_failures);
    }

    /// Polygons passed to `snap_and_fix_polygon` whose pointwise rounding is invalid, from OpenMapTiles Switzerland
    /// (most of them invalid themselves) and Monaco.
    #[test]
    fn reduce_precision_matches_java_on_tile_polygons() {
        let result = check_java_cases("tiles.txt.gz");
        assert!(result.cases > 1000);
        assert_no_failures(&result.failures);
        // 41 of the 3,084 (of 109,910) Switzerland polygons where JTS reduces an invalid input differently
        assert!(result.invalid_reduce_failures.len() <= 14, "{}", result.invalid_reduce_failures.len());
    }

    /// `SNAPROUND_CASES=/path/to/cases.txt.gz cargo test --release -- --ignored snapround_cases_from_env`: checks a
    /// case file generated outside the repo (e.g. from a whole-country dump of tile polygons) and writes the
    /// differences to `SNAPROUND_FAILURES` if set.
    #[test]
    #[ignore = "needs SNAPROUND_CASES"]
    fn snapround_cases_from_env() {
        let result = check_java_cases(&std::env::var("SNAPROUND_CASES").unwrap());
        if let Ok(out) = std::env::var("SNAPROUND_FAILURES") {
            std::fs::write(out, [result.failures.join("\n"), result.invalid_reduce_failures.join("\n")].join("\n"))
                .unwrap();
        }
        assert_no_failures(&result.failures);
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

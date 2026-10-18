//! Port of JTS's snap-rounding noder (`org.locationtech.jts.noding.snapround.SnapRoundingNoder`), which OverlayNG
//! uses to node geometries on a fixed precision grid (e.g. `GeometryPrecisionReducer`).
//!
//! Like JTS, [`snap_round`]:
//!
//! 1. Finds the intersections between the input segment strings with `MCIndexNoder` (monotone chains, segment pairs
//!    in different chains whose envelopes are within the nearness tolerance) and `SnapRoundingIntersectionAdder`:
//!    every interior intersection (`RobustLineIntersector`), and every vertex closer than `gridSize / 100` to a
//!    segment of another chain, becomes a node of the segments involved and a hot pixel.
//! 2. Adds a hot pixel for every input vertex.
//! 3. Rounds the noded strings to the grid and snaps every noded segment to each hot pixel it passes through
//!    (`HotPixel.intersects`: the pixel is the unit square around the grid point, open on its top and right sides),
//!    ordering the snapped pixels along a segment the way JTS's `SegmentNodeList` does.
//!
//! The output is the snapped coordinates of each string on the integer grid (`round(x * scale)`), which is what
//! JTS's noded substrings join up to. Building the topology from them (JTS's overlay graph) is left to the caller.
//!
//! JTS also marks hot pixels as nodes and adds vertex nodes, but that only changes where the noded strings are split,
//! not their coordinates, so it is not needed here.

use crate::geo::cg_algorithms::{LineIntersector, distance, orientation_index, point_to_segment};
use crate::geo::precision_model::java_round;
use geo_types::Coord;

/// A point on the integer grid (coordinates multiplied by the precision model's scale).
pub type Pixel = (i64, i64);

/// JTS `HotPixel`: the unit square (in grid units) centered on a grid point, with open top and right sides.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct HotPixel {
    pub x: i64,
    pub y: i64,
}

impl HotPixel {
    const TOLERANCE: f64 = 0.5;

    /// JTS `HotPixel.intersectsScaled`: whether the segment `p0`-`p1` (in scaled grid coordinates) intersects this
    /// pixel.
    pub fn intersects(&self, p0: (f64, f64), p1: (f64, f64)) -> bool {
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
        let orient =
            |x: f64, y: f64| orientation_index(Coord { x: p.0, y: p.1 }, Coord { x: q.0, y: q.1 }, Coord { x, y });
        let orient_ul = orient(minx, maxy);
        if orient_ul == 0 {
            // upward segment does not intersect pixel interior
            return p.1 >= q.1;
        }
        let orient_ur = orient(maxx, maxy);
        if orient_ur == 0 {
            // downward segment does not intersect pixel interior
            return p.1 <= q.1;
        }
        // check crossing Top side
        if orient_ul != orient_ur {
            return true;
        }
        let orient_ll = orient(minx, miny);
        if orient_ll == 0 {
            // segment crossed LL corner, which is the only one in pixel interior
            return true;
        }
        // check crossing Left side
        if orient_ll != orient_ul {
            return true;
        }
        let orient_lr = orient(maxx, miny);
        if orient_lr == 0 {
            // upward segment does not intersect pixel interior
            return p.1 >= q.1;
        }
        // check crossing Bottom side or Right side
        orient_ll != orient_lr || orient_lr != orient_ur
    }
}

/// Hot pixels sorted by `(x, y)` and by `(y, x)`, queried by envelope. Each query scans whichever of the two sorted
/// ranges (pixels with x in range, or with y in range) is shorter. Tile polygons have a few hundred vertices, where
/// this is much cheaper than building a tree per call (JTS uses a k-d tree; the visiting order does not matter).
pub struct HotPixelIndex {
    /// `x` of the pixels sorted by `(x, y)`, then their `y`
    xs: Vec<i64>,
    ys_by_x: Vec<i64>,
    /// `y` of the pixels sorted by `(y, x)`, then their `x`
    ys: Vec<i64>,
    xs_by_y: Vec<i64>,
}

impl HotPixelIndex {
    /// `pixels` must be sorted by `(x, y)` and deduplicated.
    pub fn new(pixels: &[Pixel]) -> HotPixelIndex {
        let xs = pixels.iter().map(|p| p.0).collect();
        let ys_by_x = pixels.iter().map(|p| p.1).collect();
        let mut by_y = pixels.to_vec();
        by_y.sort_unstable_by_key(|p| (p.1, p.0));
        HotPixelIndex {
            xs,
            ys_by_x,
            ys: by_y.iter().map(|p| p.1).collect(),
            xs_by_y: by_y.iter().map(|p| p.0).collect(),
        }
    }

    /// Calls `f` for every pixel inside (or on the boundary of) the envelope `[min, max]`.
    #[inline]
    pub fn for_each_in(&self, min: (f64, f64), max: (f64, f64), mut f: impl FnMut(HotPixel)) {
        // the integer coordinates in range (saturating casts)
        let (min_x, max_x) = (min.0.ceil() as i64, max.0.floor() as i64);
        let (min_y, max_y) = (min.1.ceil() as i64, max.1.floor() as i64);
        let x_start = self.xs.partition_point(|&x| x < min_x);
        let x_end = self.xs.partition_point(|&x| x <= max_x).max(x_start);
        // a short x range is scanned without looking for the y range
        let use_x = x_end - x_start <= 8 || {
            let y_start = self.ys.partition_point(|&y| y < min_y);
            let y_end = self.ys.partition_point(|&y| y <= max_y).max(y_start);
            if x_end - x_start <= y_end - y_start {
                true
            } else {
                for i in y_start..y_end {
                    let x = self.xs_by_y[i];
                    if min_x <= x && x <= max_x {
                        f(HotPixel { x, y: self.ys[i] });
                    }
                }
                false
            }
        };
        if use_x {
            for i in x_start..x_end {
                let y = self.ys_by_x[i];
                if min_y <= y && y <= max_y {
                    f(HotPixel { x: self.xs[i], y });
                }
            }
        }
    }

    /// Calls `f` for every pixel that could intersect the segment `p0`-`p1` (scaled grid coordinates).
    #[inline]
    pub fn for_each_near_segment(&self, p0: (f64, f64), p1: (f64, f64), f: impl FnMut(HotPixel)) {
        // like JTS, expand the query by a full pixel (more than the pixel half-width) to be safe
        let min = (p0.0.min(p1.0) - 1.0, p0.1.min(p1.1) - 1.0);
        let max = (p0.0.max(p1.0) + 1.0, p0.1.max(p1.1) + 1.0);
        self.for_each_in(min, max, f);
    }
}

/// JTS `Octant.octant(dx, dy)` (0 for a zero-length segment, like `NodedSegmentString.safeOctant`).
#[inline]
fn octant(dx: f64, dy: f64) -> u8 {
    let (adx, ady) = (dx.abs(), dy.abs());
    if dx == 0.0 && dy == 0.0 {
        0
    } else if dx >= 0.0 {
        if dy >= 0.0 {
            if adx >= ady { 0 } else { 1 }
        } else if adx >= ady {
            7
        } else {
            6
        }
    } else if dy >= 0.0 {
        if adx >= ady { 3 } else { 2 }
    } else if adx >= ady {
        4
    } else {
        5
    }
}

/// JTS `SegmentPointComparator.compare`: the order of two (distinct) nodes along a segment in octant `octant`.
#[inline]
fn segment_point_compare(octant: u8, p0: (f64, f64), p1: (f64, f64)) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let sign = |a: f64, b: f64| -> i8 {
        if a < b {
            -1
        } else if a > b {
            1
        } else {
            0
        }
    };
    let (xs, ys) = (sign(p0.0, p1.0), sign(p0.1, p1.1));
    let (a, b) = match octant {
        0 => (xs, ys),
        1 => (ys, xs),
        2 => (ys, -xs),
        3 => (-xs, ys),
        4 => (-xs, -ys),
        5 => (-ys, -xs),
        6 => (-ys, xs),
        _ => (xs, -ys),
    };
    let ordering = |v: i8| v.cmp(&0);
    match ordering(a) {
        Ordering::Equal => ordering(b),
        other => other,
    }
}

/// A node on a segment string (JTS `SegmentNode`): the index of the segment it is on and its coordinate.
#[derive(Clone, Copy, Debug)]
struct Node {
    segment: u32,
    coord: Coord<f64>,
}

/// JTS `SegmentNode.compareTo` for two nodes of `pts`.
#[inline]
fn compare_nodes(pts: &[Coord<f64>], a: &Node, b: &Node) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match a.segment.cmp(&b.segment) {
        Ordering::Equal => {}
        other => return other,
    }
    if a.coord == b.coord {
        return Ordering::Equal;
    }
    let s = a.segment as usize;
    // an exterior node is the segment start point, so always sorts first
    if a.coord == pts[s] {
        return Ordering::Less;
    }
    if b.coord == pts[s] {
        return Ordering::Greater;
    }
    let oct = if s + 1 < pts.len() { octant(pts[s + 1].x - pts[s].x, pts[s + 1].y - pts[s].y) } else { 0 };
    segment_point_compare(oct, (a.coord.x, a.coord.y), (b.coord.x, b.coord.y))
}

/// JTS `NodedSegmentString.addIntersection`: the node for `int_pt` on segment `segment` of `pts`.
#[inline]
fn node_at(pts: &[Coord<f64>], segment: usize, int_pt: Coord<f64>) -> Node {
    let normalized = if segment + 1 < pts.len() && int_pt == pts[segment + 1] { segment + 1 } else { segment };
    Node { segment: normalized as u32, coord: int_pt }
}

/// JTS `SegmentNodeList.getSplitCoordinates`: the coordinates of `pts` with the `nodes` inserted (in order, without
/// repeated points).
fn split_coordinates(pts: &[Coord<f64>], nodes: &mut Vec<Node>) -> Vec<Coord<f64>> {
    let n = pts.len();
    // addEndpoints
    nodes.push(Node { segment: 0, coord: pts[0] });
    nodes.push(Node { segment: (n - 1) as u32, coord: pts[n - 1] });
    nodes.sort_by(|a, b| compare_nodes(pts, a, b));
    nodes.dedup_by(|b, a| a.segment == b.segment && a.coord == b.coord);
    let mut out: Vec<Coord<f64>> = Vec::with_capacity(n + nodes.len());
    let mut push = |c: Coord<f64>| {
        if out.last() != Some(&c) {
            out.push(c);
        }
    };
    for w in nodes.windows(2) {
        // createSplitEdgePts
        let (ei0, ei1) = (w[0], w[1]);
        push(ei0.coord);
        for c in &pts[ei0.segment as usize + 1..=ei1.segment as usize] {
            push(*c);
        }
        if ei1.segment == ei0.segment || ei1.coord != pts[ei1.segment as usize] {
            push(ei1.coord);
        }
    }
    out
}

/// JTS `Quadrant.quadrant(p0, p1)` (`p0 != p1`).
#[inline]
fn quadrant(p0: Coord<f64>, p1: Coord<f64>) -> u8 {
    if p1.x >= p0.x {
        if p1.y >= p0.y { 0 } else { 3 }
    } else if p1.y >= p0.y {
        1
    } else {
        2
    }
}

/// JTS `MonotoneChainBuilder.findChainEnd`.
fn find_chain_end(pts: &[Coord<f64>], start: usize) -> usize {
    let mut safe_start = start;
    // skip any zero-length segments at the start of the sequence (since they cannot be used to establish a quadrant)
    while safe_start < pts.len() - 1 && pts[safe_start] == pts[safe_start + 1] {
        safe_start += 1;
    }
    // check if there are NO non-zero-length segments
    if safe_start >= pts.len() - 1 {
        return pts.len() - 1;
    }
    // determine overall quadrant for chain (which is the starting quadrant)
    let chain_quad = quadrant(pts[safe_start], pts[safe_start + 1]);
    let mut last = start + 1;
    while last < pts.len() {
        // skip zero-length segments, but include them in the chain
        if pts[last - 1] != pts[last] && quadrant(pts[last - 1], pts[last]) != chain_quad {
            break;
        }
        last += 1;
    }
    last - 1
}

/// A JTS `MonotoneChain`: segments `start..end` of string `string`, with its envelope.
#[derive(Clone, Copy)]
struct Chain {
    string: u32,
    start: u32,
    end: u32,
    min: (f64, f64),
    max: (f64, f64),
}

/// JTS `MonotoneChainBuilder.getChains` for every string, in order (the order defines JTS's chain ids).
fn monotone_chains(strings: &[&[Coord<f64>]]) -> Vec<Chain> {
    let mut chains = Vec::new();
    for (s, pts) in strings.iter().enumerate() {
        if pts.is_empty() {
            continue;
        }
        let mut start = 0;
        loop {
            let end = find_chain_end(pts, start);
            let (a, b) = (pts[start], pts[end]);
            chains.push(Chain {
                string: s as u32,
                start: start as u32,
                end: end as u32,
                min: (a.x.min(b.x), a.y.min(b.y)),
                max: (a.x.max(b.x), a.y.max(b.y)),
            });
            start = end;
            if start >= pts.len() - 1 {
                break;
            }
        }
    }
    chains
}

/// JTS `MonotoneChain.overlaps(p1, p2, q1, q2, overlapTolerance)`.
#[inline]
fn envelopes_overlap(p1: Coord<f64>, p2: Coord<f64>, q1: Coord<f64>, q2: Coord<f64>, tolerance: f64) -> bool {
    let (minq, maxq, minp, maxp) = (q1.x.min(q2.x), q1.x.max(q2.x), p1.x.min(p2.x), p1.x.max(p2.x));
    if minp > maxq + tolerance || maxp < minq - tolerance {
        return false;
    }
    let (minq, maxq, minp, maxp) = (q1.y.min(q2.y), q1.y.max(q2.y), p1.y.min(p2.y), p1.y.max(p2.y));
    !(minp > maxq + tolerance || maxp < minq - tolerance)
}

/// JTS `MonotoneChain.computeOverlaps`: calls `f(segment0, segment1)` for the pairs of segments of the two chain
/// sections that may be within `tolerance`.
#[allow(clippy::too_many_arguments)]
fn compute_overlaps(
    pts0: &[Coord<f64>],
    start0: usize,
    end0: usize,
    pts1: &[Coord<f64>],
    start1: usize,
    end1: usize,
    tolerance: f64,
    f: &mut impl FnMut(usize, usize),
) {
    // terminating condition for the recursion
    if end0 - start0 == 1 && end1 - start1 == 1 {
        f(start0, start1);
        return;
    }
    // nothing to do if the envelopes of these subchains don't overlap
    if !envelopes_overlap(pts0[start0], pts0[end0], pts1[start1], pts1[end1], tolerance) {
        return;
    }
    // the chains overlap, so split each in half and iterate (binary search)
    let mid0 = (start0 + end0) / 2;
    let mid1 = (start1 + end1) / 2;
    if start0 < mid0 {
        if start1 < mid1 {
            compute_overlaps(pts0, start0, mid0, pts1, start1, mid1, tolerance, f);
        }
        if mid1 < end1 {
            compute_overlaps(pts0, start0, mid0, pts1, mid1, end1, tolerance, f);
        }
    }
    if mid0 < end0 {
        if start1 < mid1 {
            compute_overlaps(pts0, mid0, end0, pts1, start1, mid1, tolerance, f);
        }
        if mid1 < end1 {
            compute_overlaps(pts0, mid0, end0, pts1, mid1, end1, tolerance, f);
        }
    }
}

/// The result of the intersection phase: the intersection points (JTS `SnapRoundingIntersectionAdder`) and the
/// nodes they add to each string.
struct Intersections {
    points: Vec<Coord<f64>>,
    nodes: Vec<Vec<Node>>,
}

/// JTS `SnapRoundingIntersectionAdder.processNearVertex`.
#[inline]
/// `(edge, segment)` is the segment `p0`-`p1`.
fn process_near_vertex(
    p: Coord<f64>,
    (edge, segment): (usize, usize),
    (p0, p1): (Coord<f64>, Coord<f64>),
    tolerance: f64,
    strings: &[&[Coord<f64>]],
    out: &mut Intersections,
) {
    // fast rejection of vertices far from the segment's envelope (with a margin, so that it never rejects a vertex
    // that the floating point distance below would find within the tolerance)
    let margin = 2.0 * tolerance;
    if p.x < p0.x.min(p1.x) - margin
        || p.x > p0.x.max(p1.x) + margin
        || p.y < p0.y.min(p1.y) - margin
        || p.y > p0.y.max(p1.y) + margin
    {
        return;
    }
    // don't add intersection if candidate vertex is near endpoints of segment (this also handles the case of
    // adjacent segments and the closing segment of a ring)
    if distance(p, p0) < tolerance || distance(p, p1) < tolerance {
        return;
    }
    if point_to_segment(p, p0, p1) < tolerance {
        out.points.push(p);
        out.nodes[edge].push(node_at(strings[edge], segment, p));
    }
}

/// JTS `MCIndexNoder(SnapRoundingIntersectionAdder(tolerance), tolerance).computeNodes(strings)`.
fn find_intersections(strings: &[&[Coord<f64>]], tolerance: f64) -> Intersections {
    let mut chains = monotone_chains(strings);
    let mut out = Intersections { points: Vec::new(), nodes: vec![Vec::new(); strings.len()] };
    let mut li = LineIntersector::new();
    // chain ids are the indexes in `chains`; candidate chain pairs come from a sweep over the chain envelopes
    // expanded by the tolerance (JTS uses an HPRtree; only the set of pairs matters, not their order)
    let mut order: Vec<u32> = (0..chains.len() as u32).collect();
    order.sort_unstable_by(|&a, &b| chains[a as usize].min.0.total_cmp(&chains[b as usize].min.0));
    let expand = 2.0 * tolerance;
    for c in &mut chains {
        // guard against NaN envelopes making the sweep skip pairs
        if c.min.0.is_nan() {
            c.min.0 = f64::NEG_INFINITY;
        }
    }
    let mut pairs: Vec<(u32, u32)> = Vec::new();
    for (n, &i) in order.iter().enumerate() {
        let a = chains[i as usize];
        for &j in &order[n + 1..] {
            let b = chains[j as usize];
            if b.min.0 > a.max.0 + expand {
                break;
            }
            if b.min.1 > a.max.1 + expand || b.max.1 < a.min.1 - expand || b.max.0 < a.min.0 - expand {
                continue;
            }
            pairs.push(if i < j { (i, j) } else { (j, i) });
        }
    }
    for (i, j) in pairs {
        let (query, test) = (chains[i as usize], chains[j as usize]);
        let (e0, e1) = (query.string as usize, test.string as usize);
        let (pts0, pts1) = (strings[e0], strings[e1]);
        compute_overlaps(
            pts0,
            query.start as usize,
            query.end as usize,
            pts1,
            test.start as usize,
            test.end as usize,
            tolerance,
            &mut |s0, s1| {
                // SnapRoundingIntersectionAdder.processIntersections
                if e0 == e1 && s0 == s1 {
                    return;
                }
                let (p00, p01, p10, p11) = (pts0[s0], pts0[s0 + 1], pts1[s1], pts1[s1 + 1]);
                // segments whose envelopes are further apart than the tolerance (with a margin) can neither
                // intersect nor have a vertex near the other segment (JTS's chain recursion does not test leaves)
                if !envelopes_overlap(p00, p01, p10, p11, 2.0 * tolerance) {
                    return;
                }
                // two segments that share exactly one endpoint and are not collinear only intersect there, which is
                // not an interior intersection (saves the intersection test for consecutive segments)
                let meet_at_endpoint_only = (p01 == p10 && orientation_index(p00, p01, p11) != 0)
                    || (p00 == p11 && orientation_index(p10, p11, p01) != 0);
                if !meet_at_endpoint_only
                    && ({
                        li.compute_intersection(p00, p01, p10, p11);
                        li.has_intersection() && li.is_interior_intersection()
                    })
                {
                    for k in 0..li.intersection_num() as usize {
                        out.points.push(li.intersection(k));
                    }
                    for k in 0..li.intersection_num() as usize {
                        out.nodes[e0].push(node_at(pts0, s0, li.intersection(k)));
                    }
                    for k in 0..li.intersection_num() as usize {
                        out.nodes[e1].push(node_at(pts1, s1, li.intersection(k)));
                    }
                    return;
                }
                process_near_vertex(p00, (e1, s1), (p10, p11), tolerance, strings, &mut out);
                process_near_vertex(p01, (e1, s1), (p10, p11), tolerance, strings, &mut out);
                process_near_vertex(p10, (e0, s0), (p00, p01), tolerance, strings, &mut out);
                process_near_vertex(p11, (e0, s0), (p00, p01), tolerance, strings, &mut out);
            },
        );
    }
    out
}

/// Snap-rounds `strings` (each without repeated consecutive points and with at least 2 points; rings closed) to
/// the grid with `scale` cells per unit, like JTS `SnapRoundingNoder` with a fixed `PrecisionModel(scale)`.
///
/// Returns the snapped coordinates of each string in grid units (`None` where the string collapses to a single
/// point), without repeated consecutive points. A closed input string stays closed.
pub fn snap_round(strings: &[&[Coord<f64>]], scale: f64) -> Vec<Option<Vec<Pixel>>> {
    let round = |c: Coord<f64>| -> Pixel { (java_round(c.x * scale) as i64, java_round(c.y * scale) as i64) };
    let grid_size = 1.0 / scale;
    let mut intersections = find_intersections(strings, grid_size / 100.0);
    // hot pixels: the intersection points and the vertices
    let n: usize = strings.iter().map(|s| s.len()).sum();
    let mut pixels: Vec<Pixel> = Vec::with_capacity(n + intersections.points.len());
    pixels.extend(intersections.points.iter().map(|c| round(*c)));
    pixels.extend(strings.iter().flat_map(|s| s.iter().map(|c| round(*c))));
    pixels.sort_unstable();
    pixels.dedup();
    let index = HotPixelIndex::new(&pixels);
    let mut result = Vec::with_capacity(strings.len());
    let mut snapped: Vec<(Pixel, (f64, f64))> = Vec::new();
    let mut noded_storage: Vec<Coord<f64>>;
    for (s, pts) in strings.iter().enumerate() {
        // SnapRoundingNoder.computeSegmentSnaps
        let nodes = &mut intersections.nodes[s];
        let pts: &[Coord<f64>] = if nodes.is_empty() {
            pts
        } else {
            noded_storage = split_coordinates(pts, nodes);
            &noded_storage
        };
        let keys: Vec<Pixel> = pts.iter().map(|c| round(*c)).collect();
        let mut out: Vec<Pixel> = Vec::with_capacity(pts.len());
        for &k in &keys {
            if out.last() != Some(&k) {
                out.push(k);
            }
        }
        // if complete collapse this edge can be eliminated
        if out.len() <= 1 {
            result.push(None);
            continue;
        }
        let rounded_len = out.len();
        let mut contour: Vec<Pixel> = Vec::with_capacity(rounded_len + 8);
        contour.push(out[0]);
        let mut current = 0;
        for i in 0..pts.len() - 1 {
            let next = keys[i + 1];
            if next == out[current] {
                continue;
            }
            let (from, to) = (out[current], out[current + 1]);
            debug_assert_eq!(next, to);
            // snapSegment: every hot pixel the segment passes through, other than the pixels of its own endpoints
            let p0 = (pts[i].x * scale, pts[i].y * scale);
            let p1 = (pts[i + 1].x * scale, pts[i + 1].y * scale);
            snapped.clear();
            let (dx, dy) = (p1.0 - p0.0, p1.1 - p0.1);
            // pixels whose center is further than 0.75 from the segment's line cannot touch it (half the pixel
            // diagonal is ~0.707): a cheap rejection before the exact test
            let max_cross = 0.75 * dx.hypot(dy);
            index.for_each_near_segment(p0, p1, |hp| {
                let key = (hp.x, hp.y);
                let cross = dx * (hp.y as f64 - p0.1) - dy * (hp.x as f64 - p0.0);
                if cross.abs() <= max_cross && key != from && key != to && hp.intersects(p0, p1) {
                    snapped.push((key, (key.0 as f64 / scale, key.1 as f64 / scale)));
                }
            });
            if snapped.len() > 1 {
                // SegmentNodeList order along the rounded segment
                let a = (from.0 as f64 / scale, from.1 as f64 / scale);
                let b = (to.0 as f64 / scale, to.1 as f64 / scale);
                let oct = octant(b.0 - a.0, b.1 - a.1);
                snapped.sort_by(|x, y| segment_point_compare(oct, x.1, y.1));
            }
            for &(key, _) in &snapped {
                if contour.last() != Some(&key) {
                    contour.push(key);
                }
            }
            if contour.last() != Some(&to) {
                contour.push(to);
            }
            current += 1;
        }
        debug_assert_eq!(current + 1, rounded_len);
        result.push(Some(contour));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(x: f64, y: f64) -> Coord<f64> {
        Coord { x, y }
    }

    #[test]
    fn crossing_segments_are_noded_at_the_intersection_pixel() {
        // two segments crossing at (0.5, 0.5): the crossing becomes a hot pixel both are snapped to
        let a = [c(0.0, 0.0), c(1.0, 1.0)];
        let b = [c(0.0, 1.0), c(1.0, 0.0)];
        let result = snap_round(&[&a, &b], 4.0);
        assert_eq!(Some(vec![(0, 0), (2, 2), (4, 4)]), result[0]);
        assert_eq!(Some(vec![(0, 4), (2, 2), (4, 0)]), result[1]);
    }

    #[test]
    fn segment_snaps_to_nearby_vertex_pixel() {
        // the vertex (5.1, 0.3) is within half a pixel of the segment, so the segment is snapped to its pixel
        let a = [c(0.0, 0.0), c(10.0, 0.0)];
        let b = [c(5.1, 0.3), c(5.1, 5.0)];
        let result = snap_round(&[&a, &b], 1.0);
        assert_eq!(Some(vec![(0, 0), (5, 0), (10, 0)]), result[0]);
        assert_eq!(Some(vec![(5, 0), (5, 5)]), result[1]);
    }

    #[test]
    fn collapsed_string_is_removed() {
        let a = [c(0.0, 0.0), c(0.1, 0.1)];
        assert_eq!(vec![None], snap_round(&[&a], 1.0));
    }

    #[test]
    fn nodes_are_ordered_by_octant() {
        assert_eq!(std::cmp::Ordering::Less, segment_point_compare(0, (0.0, 0.0), (1.0, 0.0)));
        assert_eq!(std::cmp::Ordering::Greater, segment_point_compare(4, (0.0, 0.0), (1.0, 0.0)));
        assert_eq!(std::cmp::Ordering::Less, segment_point_compare(1, (5.0, 0.0), (0.0, 1.0)));
        assert_eq!(0, octant(2.0, 1.0));
        assert_eq!(5, octant(-1.0, -2.0));
    }
}

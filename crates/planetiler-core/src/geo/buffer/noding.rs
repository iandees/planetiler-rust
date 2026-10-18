//! Emulation of JTS's floating point noding of buffer curves (`MCIndexNoder` + `IntersectionAdder`) and its
//! validation (`FastNodingValidator`), used to predict when JTS's `BufferOp` falls back to snap-rounding at a fixed
//! precision.
//!
//! JTS first buffers in floating point: it nodes the raw offset curves by adding every pairwise segment intersection
//! (computed in double-double arithmetic and rounded to doubles) as a node, and builds the result from the noded
//! curves. When the rounded nodes leave the curves not fully noded (two noded segments still cross), building the
//! result throws a `TopologyException` and `BufferOp` retries with snap-rounding on a 10^-k grid
//! (`bufferReducedPrecision`). The two give slightly different vertices (the fallback output is on the 10^-k grid),
//! so the port needs to know which one JTS used. Checking whether the floating noding is valid predicts the
//! fallback: on the Boston building tiles it flags all 73 unbuffer fallbacks, plus 6 of ~2200 that JTS managed
//! anyway.
//!
//! Differences from JTS: candidate segment pairs come from a sweep over segment envelopes instead of monotone chains
//! (the same pairs up to ones that can only have trivial intersections), and nodes along a segment are ordered by
//! their projection on the segment instead of `SegmentPointComparator`.

use crate::geo::cg_algorithms::LineIntersector;
use geo_types::Coord;

/// A node on a curve: index of the segment it is on, its position along that segment and its coordinate.
#[derive(Clone, Copy, Debug)]
struct Node {
    segment: usize,
    t: f64,
    coord: Coord<f64>,
}

fn param(a: Coord<f64>, b: Coord<f64>, p: Coord<f64>) -> f64 {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let len2 = dx * dx + dy * dy;
    if len2 == 0.0 { 0.0 } else { ((p.x - a.x) * dx + (p.y - a.y) * dy) / len2 }
}

/// Calls `f(i, j)` once for every pair `i < j` of segments (indices into `segments`) whose envelopes intersect.
fn for_each_candidate_pair(segments: &[(Coord<f64>, Coord<f64>)], f: impl FnMut(usize, usize)) {
    for_each_candidate_pair_with(segments, None, f);
}

/// [`for_each_candidate_pair`], limited to the pairs where at least one segment is `selected` (if given).
///
/// Uses a sweep over the segments sorted by their minimum x.
fn for_each_candidate_pair_with(
    segments: &[(Coord<f64>, Coord<f64>)],
    selected: Option<&[bool]>,
    mut f: impl FnMut(usize, usize),
) {
    let is_selected = |i: usize| selected.is_none_or(|s| s[i]);
    // (min x, max x, min y, max y, index)
    let mut boxes: Vec<(f64, f64, f64, f64, usize)> = segments
        .iter()
        .enumerate()
        .map(|(i, (a, b))| (a.x.min(b.x), a.x.max(b.x), a.y.min(b.y), a.y.max(b.y), i))
        .collect();
    boxes.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
    for (n, &(_, max_x, min_y, max_y, i)) in boxes.iter().enumerate() {
        let selected_i = is_selected(i);
        for &(other_min_x, _, other_min_y, other_max_y, j) in &boxes[n + 1..] {
            if other_min_x > max_x {
                break;
            }
            if other_min_y > max_y || other_max_y < min_y || !(selected_i || is_selected(j)) {
                continue;
            }
            if i < j { f(i, j) } else { f(j, i) }
        }
    }
}

type Segment = (Coord<f64>, Coord<f64>);

/// Whether consecutive segments `p00-p01` and `p10-p11` of a curve (`p01 == p10`) intersect only at their shared
/// vertex, i.e. `LineIntersector` would find exactly that one point: true unless the three points are collinear
/// (where the segments may overlap). Saves the full intersection test for most adjacent pairs.
#[inline]
fn adjacent_meet_at_vertex_only(p00: Coord<f64>, p01: Coord<f64>, p10: Coord<f64>, p11: Coord<f64>) -> bool {
    p01 == p10 && crate::geo::cg_algorithms::orientation_index(p00, p01, p11) != 0
}

/// Segments of every curve, with the curve index and segment index of each.
fn segments_of(curves: &[Vec<Coord<f64>>]) -> (Vec<Segment>, Vec<(usize, usize)>) {
    let mut segments = Vec::new();
    let mut ids = Vec::new();
    for (c, curve) in curves.iter().enumerate() {
        for (s, w) in curve.windows(2).enumerate() {
            segments.push((w[0], w[1]));
            ids.push((c, s));
        }
    }
    (segments, ids)
}

/// JTS `NodedSegmentString.addIntersection`: the node for `int_pt` on segment `segment` of `pts`.
fn node_at(pts: &[Coord<f64>], segment: usize, int_pt: Coord<f64>) -> Node {
    let mut normalized = segment;
    if segment + 1 < pts.len() && int_pt == pts[segment + 1] {
        normalized = segment + 1;
    }
    let t = if normalized + 1 < pts.len() { param(pts[normalized], pts[normalized + 1], int_pt) } else { 0.0 };
    Node { segment: normalized, t, coord: int_pt }
}

/// JTS `MCIndexNoder` with `IntersectionAdder` (floating precision): the noded substrings of `curves`, or `None` if
/// no curve has an intersection node (other than its endpoints), in which case the noding is trivially valid.
fn noded_substrings(curves: &[Vec<Coord<f64>>]) -> Option<Vec<Vec<Coord<f64>>>> {
    let (segments, ids) = segments_of(curves);
    let mut nodes: Vec<Vec<Node>> = vec![Vec::new(); curves.len()];
    let mut li = LineIntersector::new();
    for_each_candidate_pair(&segments, |i, j| {
        let ((c0, s0), (c1, s1)) = (ids[i], ids[j]);
        let ((p00, p01), (p10, p11)) = (segments[i], segments[j]);
        if c0 == c1 && s1 == s0 + 1 && adjacent_meet_at_vertex_only(p00, p01, p10, p11) {
            return;
        }
        li.compute_intersection(p00, p01, p10, p11);
        if !li.has_intersection() {
            return;
        }
        // IntersectionAdder.isTrivialIntersection: the shared vertex of adjacent segments (JTS's check for the
        // first and last segment of a closed curve never matches, but those only share the curve's endpoints,
        // which are nodes anyway)
        if c0 == c1 && li.intersection_num() == 1 && s0.abs_diff(s1) == 1 {
            return;
        }
        for k in 0..li.intersection_num() as usize {
            let p = li.intersection(k);
            nodes[c0].push(node_at(&curves[c0], s0, p));
            nodes[c1].push(node_at(&curves[c1], s1, p));
        }
    });
    if nodes.iter().all(|n| n.is_empty()) {
        // every intersection was the shared vertex of adjacent segments
        return None;
    }
    let mut result = Vec::new();
    for (curve, mut nodes) in curves.iter().zip(nodes) {
        if curve.len() < 2 {
            continue;
        }
        let n = curve.len();
        // SegmentNodeList.addEndpoints
        nodes.push(Node { segment: 0, t: 0.0, coord: curve[0] });
        nodes.push(Node { segment: n - 1, t: 0.0, coord: curve[n - 1] });
        sort_dedup(&mut nodes);
        // SegmentNodeList.addCollapsedNodes
        let mut collapsed = Vec::new();
        for w in nodes.windows(2) {
            let (ei0, ei1) = (w[0], w[1]);
            if ei0.coord == ei1.coord {
                let mut between = ei1.segment - ei0.segment;
                if ei1.coord == curve[ei1.segment] {
                    between -= 1;
                }
                if between == 1 {
                    collapsed.push(ei0.segment + 1);
                }
            }
        }
        for i in 0..n.saturating_sub(2) {
            if curve[i] == curve[i + 2] {
                collapsed.push(i + 1);
            }
        }
        if !collapsed.is_empty() {
            nodes.extend(collapsed.into_iter().map(|i| Node { segment: i, t: 0.0, coord: curve[i] }));
            sort_dedup(&mut nodes);
        }
        // SegmentNodeList.createSplitEdgePts
        for w in nodes.windows(2) {
            let (ei0, ei1) = (w[0], w[1]);
            let npts = ei1.segment - ei0.segment + 2;
            if npts == 2 {
                result.push(vec![ei0.coord, ei1.coord]);
                continue;
            }
            let use_int_pt1 = ei1.coord != curve[ei1.segment];
            let mut pts = Vec::with_capacity(npts);
            pts.push(ei0.coord);
            pts.extend_from_slice(&curve[ei0.segment + 1..=ei1.segment]);
            if use_int_pt1 {
                pts.push(ei1.coord);
            }
            result.push(pts);
        }
    }
    Some(result)
}

/// Sorts nodes along the curve and merges nodes with the same segment and coordinate (JTS's node map).
fn sort_dedup(nodes: &mut Vec<Node>) {
    nodes.sort_by(|a, b| a.segment.cmp(&b.segment).then(a.t.total_cmp(&b.t)));
    nodes.dedup_by(|b, a| a.segment == b.segment && a.coord == b.coord);
}

/// JTS `FastNodingValidator.isValid` on `strings` (noded substrings): no two segments intersect in their
/// interiors, and no vertex that is not an endpoint of a string touches another segment's vertex.
///
/// Only pairs involving the first or last segment of a string are checked: the other segments are unchanged input
/// segments, and two unchanged segments were already found not to intersect (other than at a shared vertex of
/// adjacent segments) when noding, since every other intersection would have split them.
fn is_noding_valid(strings: &[Vec<Coord<f64>>]) -> bool {
    let (segments, ids) = segments_of(strings);
    let selected: Vec<bool> = ids.iter().map(|&(e, s)| s == 0 || s + 2 == strings[e].len()).collect();
    let mut li = LineIntersector::new();
    let mut valid = true;
    for_each_candidate_pair_with(&segments, Some(&selected), |i, j| {
        if !valid {
            return;
        }
        let ((e0, s0), (e1, s1)) = (ids[i], ids[j]);
        let ((p00, p01), (p10, p11)) = (segments[i], segments[j]);
        let is_end00 = s0 == 0;
        let is_end01 = s0 + 2 == strings[e0].len();
        let is_end10 = s1 == 0;
        let is_end11 = s1 + 2 == strings[e1].len();
        if e0 == e1 && s1 == s0 + 1 && adjacent_meet_at_vertex_only(p00, p01, p10, p11) {
            // the only intersection is the shared endpoint, and adjacent segments skip the vertex checks
            return;
        }
        li.compute_intersection(p00, p01, p10, p11);
        if li.has_intersection() && li.is_interior_intersection() {
            valid = false;
            return;
        }
        let adjacent = e0 == e1 && s0.abs_diff(s1) <= 1;
        if !adjacent {
            let vertex_int = |a: Coord<f64>, b: Coord<f64>, end_a: bool, end_b: bool| !(end_a && end_b) && a == b;
            if vertex_int(p00, p10, is_end00, is_end10)
                || vertex_int(p00, p11, is_end00, is_end11)
                || vertex_int(p01, p10, is_end01, is_end10)
                || vertex_int(p01, p11, is_end01, is_end11)
            {
                valid = false;
            }
        }
    });
    valid
}

/// Whether JTS's floating point noding of the raw buffer `curves` produces a valid noding (if not, JTS's
/// `BufferOp` falls back to snap-rounding at a fixed precision).
pub fn floating_noding_is_valid(curves: &[Vec<Coord<f64>>]) -> bool {
    noded_substrings(curves).is_none_or(|strings| is_noding_valid(&strings))
}

/// Whether the closed `curve` is simple: no two of its segments intersect except consecutive segments (and the
/// first and last one) at their shared vertex, and it has no repeated consecutive vertices. A simple curve is a
/// single noded string for JTS's floating point noding, which is then trivially valid
/// ([`floating_noding_is_valid`] is true).
pub fn is_simple_closed_curve(curve: &[Coord<f64>]) -> bool {
    let n = curve.len();
    if n < 4 || curve[0] != curve[n - 1] || curve.windows(2).any(|w| w[0] == w[1]) {
        return false;
    }
    let last = n - 2;
    // (min x, max x, min y, max y, segment)
    let mut boxes: Vec<(f64, f64, f64, f64, usize)> = curve
        .windows(2)
        .enumerate()
        .map(|(i, w)| (w[0].x.min(w[1].x), w[0].x.max(w[1].x), w[0].y.min(w[1].y), w[0].y.max(w[1].y), i))
        .collect();
    boxes.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
    let mut li = LineIntersector::new();
    for (k, &(_, max_x, min_y, max_y, a)) in boxes.iter().enumerate() {
        for &(other_min_x, _, other_min_y, other_max_y, b) in &boxes[k + 1..] {
            if other_min_x > max_x {
                break;
            }
            if other_min_y > max_y || other_max_y < min_y {
                continue;
            }
            let (i, j) = if a < b { (a, b) } else { (b, a) };
            let (p00, p01, p10, p11) = (curve[i], curve[i + 1], curve[j], curve[j + 1]);
            if j == i + 1 && adjacent_meet_at_vertex_only(p00, p01, p10, p11) {
                continue;
            }
            li.compute_intersection(p00, p01, p10, p11);
            let num = li.intersection_num();
            if num == 0 || (num == 1 && (j == i + 1 || (i == 0 && j == last && li.intersection(0) == curve[0]))) {
                continue;
            }
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::test_utils::new_coordinate_list;

    #[test]
    fn simple_curves_are_validly_noded() {
        let square = new_coordinate_list(&[0.0, 0.0, 10.0, 0.0, 10.0, 10.0, 0.0, 10.0, 0.0, 0.0]);
        assert!(floating_noding_is_valid(std::slice::from_ref(&square)));
        // two crossing squares: the crossings are exact, so the noding is valid
        let other = new_coordinate_list(&[5.0, 5.0, 15.0, 5.0, 15.0, 15.0, 5.0, 15.0, 5.0, 5.0]);
        assert!(floating_noding_is_valid(&[square.clone(), other]));
        let strings =
            noded_substrings(&[square.clone(), new_coordinate_list(&[5.0, -5.0, 5.0, 5.0, 6.0, -5.0, 5.0, -5.0])])
                .unwrap();
        // the square is split at its 2 crossings with the triangle (plus its start), the triangle at 2
        assert_eq!(6, strings.len(), "{strings:?}");
        assert!(is_noding_valid(&strings));
        // un-noded crossing curves are not valid
        assert!(!is_noding_valid(&[square, new_coordinate_list(&[5.0, -5.0, 5.0, 5.0])]));
    }
}

//! Port of `reader/ShapefilePolygonHandler.java`: assembles the rings of a 2D polygon record from a shapefile into a
//! `MultiPolygon`, following the "Polygon" section of the
//! [ESRI Shapefile Technical Description](https://www.esri.com/content/dam/esrisites/sitecore-archive/Files/Pdfs/library/whitepapers/pdfs/shapefile.pdf).
//!
//! Upstream uses this in place of GeoTools' `PolygonHandler` because it is much faster assigning holes to shells: it
//! creates a spatial index to speed up that lookup when there are many.
//!
//! Clockwise rings are outer rings and counterclockwise rings are holes. With one outer ring, every hole goes into it.
//! With several, each hole goes into the smallest outer ring that contains it (a hole may touch its outer ring at a
//! vertex), and a hole that no outer ring contains becomes an outer ring of its own. Polygons come out in record order
//! of their outer rings followed by the ones from such orphaned holes, each with its holes in record order. Unclosed
//! rings get closed, and rings with fewer than 4 points are skipped.

use geo::InteriorPoint;
use geo::algorithm::kernels::{Kernel, Orientation, RobustKernel};
use geo_types::{Coord, LineString, MultiPolygon, Polygon};
use rstar::primitives::{GeomWithData, Rectangle};
use rstar::{AABB, RTree};

use crate::geo::envelope::Envelope;

/// Where a point is relative to a ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Location {
    Interior,
    Boundary,
    Exterior,
}

/// A ring edge indexed by its bounding box.
type Edge = GeomWithData<Rectangle<[f64; 2]>, (Coord<f64>, Coord<f64>)>;

/// Locates points in the area enclosed by a ring using an index of its edges (JTS `IndexedPointInAreaLocator`).
struct RingLocator {
    edges: RTree<Edge>,
}

impl RingLocator {
    fn new(ring: &[Coord<f64>]) -> RingLocator {
        let edges = ring
            .windows(2)
            .map(|w| {
                GeomWithData::new(
                    Rectangle::from_corners(
                        [w[0].x.min(w[1].x), w[0].y.min(w[1].y)],
                        [w[0].x.max(w[1].x), w[0].y.max(w[1].y)],
                    ),
                    (w[0], w[1]),
                )
            })
            .collect();
        RingLocator { edges: RTree::bulk_load(edges) }
    }

    /// Counts crossings of a ray from `p` to +x with the ring edges (JTS `RayCrossingCounter`), only looking at the
    /// edges that the ray can touch.
    fn locate(&self, p: Coord<f64>) -> Location {
        let mut crossings = 0usize;
        let mut on_segment = false;
        let ray = AABB::from_corners([p.x, p.y], [f64::MAX, p.y]);
        for edge in self.edges.locate_in_envelope_intersecting(ray) {
            let (p1, p2) = edge.data;
            if p1.x < p.x && p2.x < p.x {
                continue;
            }
            if p == p2 || p == p1 {
                on_segment = true;
                break;
            }
            if p1.y == p.y && p2.y == p.y {
                if p.x >= p1.x.min(p2.x) && p.x <= p1.x.max(p2.x) {
                    on_segment = true;
                    break;
                }
                continue;
            }
            if (p1.y > p.y && p2.y <= p.y) || (p2.y > p.y && p1.y <= p.y) {
                match RobustKernel::orient2d(p1, p2, p) {
                    Orientation::Collinear => {
                        on_segment = true;
                        break;
                    }
                    orientation => {
                        let left = (orientation == Orientation::CounterClockwise) != (p2.y < p1.y);
                        if left {
                            crossings += 1;
                        }
                    }
                }
            }
        }
        if on_segment {
            Location::Boundary
        } else if crossings % 2 == 1 {
            Location::Interior
        } else {
            Location::Exterior
        }
    }
}

struct Shell {
    ring: Vec<Coord<f64>>,
    envelope: Envelope,
    holes: Vec<Vec<Coord<f64>>>,
    area: Option<f64>,
    /// Built on first use, since most outer rings never get tested, and the index makes testing many holes faster.
    locator: Option<RingLocator>,
}

impl Shell {
    fn new(ring: Vec<Coord<f64>>) -> Shell {
        let envelope = ring_envelope(&ring);
        Shell { ring, envelope, holes: vec![], area: None, locator: None }
    }

    fn area(&mut self) -> f64 {
        *self.area.get_or_insert_with(|| signed_area(&self.ring).abs())
    }

    fn contains(&mut self, hole: &[Coord<f64>]) -> bool {
        let locator = self.locator.get_or_insert_with(|| RingLocator::new(&self.ring));
        let mut location = locator.locate(hole[0]);
        if location == Location::Boundary {
            // from spec "Polygon": rings may touch at vertices but not cross, so a point inside the hole is either
            // inside this ring or outside, never on it.
            let interior = Polygon::new(LineString(hole.to_vec()), vec![]).interior_point();
            location = interior.map_or(Location::Exterior, |p| locator.locate(p.0));
        }
        location != Location::Exterior
    }
}

fn ring_envelope(ring: &[Coord<f64>]) -> Envelope {
    let mut env = Envelope::null();
    for c in ring {
        env.expand_to_include(c.x, c.y);
    }
    env
}

/// Signed area of a ring (positive = counter-clockwise).
fn signed_area(ring: &[Coord<f64>]) -> f64 {
    // shift to the first point to reduce floating point error
    let origin = ring[0];
    ring.windows(2)
        .map(|w| (w[0].x - origin.x) * (w[1].y - origin.y) - (w[1].x - origin.x) * (w[0].y - origin.y))
        .sum::<f64>()
        / 2.0
}

/// JTS `Orientation.isCCW` for a closed ring with at least 4 points: a flat ring is not counterclockwise.
fn is_ccw(ring: &[Coord<f64>]) -> bool {
    // number of points without the closing endpoint
    let n = ring.len() - 1;
    // find the first highest point after a lower point, if one exists (a rising segment), otherwise the ring is flat
    let mut up_hi = ring[0];
    let mut prev_y = up_hi.y;
    let mut up_low = None;
    let mut up_hi_index = 0;
    for (i, c) in ring.iter().enumerate().skip(1) {
        if c.y > prev_y && c.y >= up_hi.y {
            up_hi = *c;
            up_hi_index = i;
            up_low = Some(ring[i - 1]);
        }
        prev_y = c.y;
    }
    let Some(up_low) = up_low else { return false };
    debug_assert!(up_hi_index > 0);
    // find the next lower point after the high point (a falling segment), which exists since the ring is not flat
    let mut down_low_index = up_hi_index;
    loop {
        down_low_index = (down_low_index + 1) % n;
        if down_low_index == up_hi_index || ring[down_low_index].y != up_hi.y {
            break;
        }
    }
    let down_low = ring[down_low_index];
    let down_hi = ring[if down_low_index > 0 { down_low_index - 1 } else { n - 1 }];
    if up_hi == down_hi {
        // a "pointed cap": the ring orientation is the orientation of the cap, unless it is an A-B-A configuration
        if up_low == up_hi || down_low == up_hi || up_low == down_low {
            return false;
        }
        RobustKernel::orient2d(up_low, up_hi, down_low) == Orientation::CounterClockwise
    } else {
        // flat cap: the direction of the flat top determines the orientation
        down_hi.x - up_hi.x < 0.0
    }
}

/// Closes `ring` if needed, or returns `None` if it has fewer than 4 points.
fn close_ring(mut ring: Vec<Coord<f64>>) -> Option<Vec<Coord<f64>>> {
    if ring.len() < 3 {
        return None;
    }
    // from spec "Polygon": rings must be closed and have at least 4 points, but close an unclosed ring instead of
    // dropping it.
    if ring.first() != ring.last() {
        ring.push(ring[0]);
    }
    (ring.len() >= 4).then_some(ring)
}

fn smallest_shell_containing(shells: &mut [Shell], hole: &[Coord<f64>]) -> Option<usize> {
    let hole_env = ring_envelope(hole);
    let mut smallest: Option<usize> = None;
    for i in 0..shells.len() {
        // The area is only computed when several outer rings contain the hole, and strict comparison picks the first
        // of exactly equal outer rings.
        if !shells[i].envelope.contains(&hole_env) {
            continue;
        }
        if let Some(s) = smallest
            && shells[i].area() >= shells[s].area()
        {
            continue;
        }
        if shells[i].contains(hole) {
            smallest = Some(i);
        }
    }
    smallest
}

/// Builds a `MultiPolygon` from the rings of a polygon record (each ring's points in record order), or `None` if no
/// ring has at least 4 points once closed.
pub(crate) fn read_polygon(rings: Vec<Vec<Coord<f64>>>) -> Option<MultiPolygon<f64>> {
    let mut shells: Vec<Shell> = vec![];
    let mut holes: Vec<Vec<Coord<f64>>> = vec![];
    for ring in rings {
        let Some(ring) = close_ring(ring) else { continue };
        // from spec "Polygon": outer rings are clockwise, holes counterclockwise, and ring order is not significant.
        if is_ccw(&ring) {
            holes.push(ring);
        } else {
            shells.push(Shell::new(ring));
        }
    }
    if shells.is_empty() && holes.is_empty() {
        return None;
    }
    // With one outer ring, every hole belongs to it, so skip the containment tests. A hole outside of it makes the
    // polygon invalid, which is what the record describes.
    let only_shell = shells.len() == 1;
    for hole in holes {
        let shell = if only_shell { Some(0) } else { smallest_shell_containing(&mut shells, &hole) };
        match shell {
            Some(i) => shells[i].holes.push(hole),
            // A hole outside every outer ring is most likely an outer ring with the wrong orientation.
            None => shells.push(Shell::new(hole)),
        }
    }
    Some(MultiPolygon(
        shells
            .into_iter()
            .map(|shell| Polygon::new(LineString(shell.ring), shell.holes.into_iter().map(LineString).collect()))
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::test_utils::{
        new_coordinate_list, new_multi_polygon, new_polygon_from_coords, new_polygon_with_holes,
        rectangle_coord_list_xy,
    };

    fn shell(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Vec<Coord<f64>> {
        let mut ring = rectangle_coord_list_xy(min_x, min_y, max_x, max_y);
        ring.reverse();
        ring
    }

    fn hole(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Vec<Coord<f64>> {
        rectangle_coord_list_xy(min_x, min_y, max_x, max_y)
    }

    /// Counterclockwise rectangle ring with `points_per_side` segments on each side (JTS `Densifier`).
    fn dense_hole(min_x: f64, min_y: f64, max_x: f64, max_y: f64, points_per_side: usize) -> Vec<Coord<f64>> {
        let corners = rectangle_coord_list_xy(min_x, min_y, max_x, max_y);
        let mut ring = vec![];
        for w in corners.windows(2) {
            for i in 0..points_per_side {
                let t = i as f64 / points_per_side as f64;
                ring.push(Coord { x: w[0].x + (w[1].x - w[0].x) * t, y: w[0].y + (w[1].y - w[0].y) * t });
            }
        }
        ring.push(corners[0]);
        ring
    }

    fn dense_shell(min_x: f64, min_y: f64, max_x: f64, max_y: f64, points_per_side: usize) -> Vec<Coord<f64>> {
        let mut ring = dense_hole(min_x, min_y, max_x, max_y, points_per_side);
        ring.reverse();
        ring
    }

    fn polygon(outer: Vec<Coord<f64>>, holes: Vec<Vec<Coord<f64>>>) -> Polygon<f64> {
        new_polygon_with_holes(outer, holes)
    }

    fn assert_reads(expected: Vec<Polygon<f64>>, rings: Vec<Vec<Coord<f64>>>) {
        assert_eq!(Some(new_multi_polygon(expected)), read_polygon(rings));
    }

    #[test]
    fn one_outer_ring_with_holes() {
        assert_reads(
            vec![polygon(shell(0., 0., 10., 10.), vec![hole(1., 1., 3., 3.), hole(5., 5., 7., 7.)])],
            vec![shell(0., 0., 10., 10.), hole(1., 1., 3., 3.), hole(5., 5., 7., 7.)],
        );
    }

    #[test]
    fn holes_split_between_outer_rings() {
        assert_reads(
            vec![
                polygon(shell(0., 0., 10., 10.), vec![hole(5., 5., 7., 7.), hole(1., 1., 3., 3.)]),
                polygon(shell(20., 0., 30., 10.), vec![hole(25., 5., 27., 7.), hole(21., 1., 23., 3.)]),
            ],
            vec![
                hole(5., 5., 7., 7.),
                shell(0., 0., 10., 10.),
                hole(25., 5., 27., 7.),
                shell(20., 0., 30., 10.),
                hole(1., 1., 3., 3.),
                hole(21., 1., 23., 3.),
            ],
        );
    }

    #[test]
    fn hole_goes_to_smallest_containing_outer_ring() {
        // island with a pond in a lake in an island, in mixed record order
        assert_reads(
            vec![
                polygon(shell(0., 0., 20., 20.), vec![hole(2., 2., 12., 12.)]),
                polygon(shell(3., 3., 8., 8.), vec![hole(4., 4., 5., 5.)]),
            ],
            vec![hole(4., 4., 5., 5.), shell(0., 0., 20., 20.), hole(2., 2., 12., 12.), shell(3., 3., 8., 8.)],
        );
    }

    #[test]
    fn orphan_hole_with_other_outer_rings() {
        assert_reads(
            vec![
                polygon(shell(0., 0., 10., 10.), vec![]),
                polygon(shell(30., 0., 40., 10.), vec![]),
                polygon(hole(20., 20., 25., 25.), vec![]),
            ],
            vec![shell(0., 0., 10., 10.), hole(20., 20., 25., 25.), shell(30., 0., 40., 10.)],
        );
    }

    #[test]
    fn every_hole_goes_into_only_outer_ring() {
        // a hole outside the only outer ring makes the polygon invalid, as the record describes
        let outer = new_coordinate_list(&[0., 0., 0., 10., 5., 10., 5., 5., 10., 5., 10., 0., 0., 0.]);
        assert_reads(
            vec![polygon(outer.clone(), vec![hole(6., 6., 8., 8.), hole(20., 20., 21., 21.)])],
            vec![outer, hole(6., 6., 8., 8.), hole(20., 20., 21., 21.)],
        );
    }

    #[test]
    fn only_holes() {
        assert_reads(
            vec![polygon(hole(0., 0., 1., 1.), vec![]), polygon(hole(5., 5., 6., 6.), vec![])],
            vec![hole(0., 0., 1., 1.), hole(5., 5., 6., 6.)],
        );
        // a later hole goes into an earlier orphan hole that contains it
        assert_reads(
            vec![polygon(hole(0., 0., 10., 10.), vec![hole(2., 2., 4., 4.)]), polygon(hole(20., 0., 21., 1.), vec![])],
            vec![hole(0., 0., 10., 10.), hole(20., 0., 21., 1.), hole(2., 2., 4., 4.)],
        );
    }

    #[test]
    fn hole_touching_small_outer_ring_vertex() {
        let triangle = new_coordinate_list(&[0., 0., 5., 2., 2., 5., 0., 0.]);
        assert_reads(
            vec![polygon(shell(-10., -10., 0., 0.), vec![]), polygon(shell(0., 0., 10., 10.), vec![triangle.clone()])],
            vec![shell(-10., -10., 0., 0.), shell(0., 0., 10., 10.), triangle],
        );
    }

    #[test]
    fn hole_touching_large_outer_ring_vertex() {
        let triangle = new_coordinate_list(&[0., 0., 50., 20., 20., 50., 0., 0.]);
        assert_reads(
            vec![
                polygon(dense_shell(-100., -100., 0., 0., 50), vec![]),
                polygon(dense_shell(0., 0., 100., 100., 50), vec![triangle.clone()]),
            ],
            vec![dense_shell(-100., -100., 0., 0., 50), dense_shell(0., 0., 100., 100., 50), triangle.clone()],
        );
        assert_reads(
            vec![
                polygon(dense_shell(0., 0., 100., 100., 50), vec![triangle.clone(), hole(10., 60., 15., 65.)]),
                polygon(dense_shell(200., 0., 300., 100., 50), vec![]),
            ],
            vec![
                dense_shell(0., 0., 100., 100., 50),
                dense_shell(200., 0., 300., 100., 50),
                triangle,
                hole(10., 60., 15., 65.),
            ],
        );
    }

    #[test]
    fn hole_with_all_points_on_outer_ring() {
        // a hole that is the outer ring reversed has no vertex off the outer ring, but its interior is inside it
        let result =
            read_polygon(vec![dense_shell(0., 0., 100., 100., 50), dense_hole(0., 0., 100., 100., 50)]).unwrap();
        assert_eq!(1, result.0.len());
        assert_eq!(1, result.0[0].interiors().len());
        // with several outer rings the locator has to test it
        let result = read_polygon(vec![
            dense_shell(0., 0., 100., 100., 50),
            dense_shell(200., 0., 300., 100., 50),
            dense_hole(0., 0., 100., 100., 50),
        ])
        .unwrap();
        assert_eq!(2, result.0.len());
        assert_eq!(1, result.0[0].interiors().len());
    }

    #[test]
    fn duplicate_outer_rings() {
        assert_reads(
            vec![
                polygon(shell(0., 0., 10., 10.), vec![hole(1., 1., 3., 3.)]),
                polygon(shell(0., 0., 10., 10.), vec![]),
            ],
            vec![shell(0., 0., 10., 10.), shell(0., 0., 10., 10.), hole(1., 1., 3., 3.)],
        );
    }

    #[test]
    fn many_holes_in_large_outer_rings() {
        let mut rings = vec![dense_shell(0., 0., 100., 100., 200), dense_shell(200., 0., 300., 100., 200)];
        for x in 0..20 {
            for y in 0..20 {
                let (x, y) = (x as f64, y as f64);
                rings.push(hole(1. + x * 5., 1. + y * 5., 3. + x * 5., 3. + y * 5.));
                rings.push(hole(201. + x * 5., 1. + y * 5., 203. + x * 5., 3. + y * 5.));
            }
        }
        let result = read_polygon(rings).unwrap();
        assert_eq!(2, result.0.len());
        assert_eq!(400, result.0[0].interiors().len());
        assert_eq!(400, result.0[1].interiors().len());
        // holes are in record order, alternating between the 2 outer rings
        assert_eq!(hole(1., 1., 3., 3.), result.0[0].interiors()[0].0);
        assert_eq!(hole(201., 1., 203., 3.), result.0[1].interiors()[0].0);
        assert_eq!(hole(1., 6., 3., 8.), result.0[0].interiors()[1].0);
    }

    #[test]
    fn closes_unclosed_rings() {
        assert_reads(
            vec![polygon(shell(0., 0., 10., 10.), vec![hole(1., 1., 3., 3.)])],
            vec![
                new_coordinate_list(&[0., 0., 0., 10., 10., 10., 10., 0.]),
                new_coordinate_list(&[1., 1., 3., 1., 3., 3., 1., 3.]),
            ],
        );
    }

    #[test]
    fn skips_short_and_empty_rings() {
        assert_reads(
            vec![polygon(shell(0., 0., 10., 10.), vec![])],
            vec![
                vec![],
                shell(0., 0., 10., 10.),
                new_coordinate_list(&[1., 1., 2., 2., 1., 1.]),
                vec![],
                new_coordinate_list(&[5., 5., 6., 6.]),
            ],
        );
    }

    #[test]
    fn record_without_usable_ring_is_none() {
        assert_eq!(None, read_polygon(vec![vec![], new_coordinate_list(&[1., 1., 2., 2., 1., 1.])]));
        assert_eq!(None, read_polygon(vec![]));
    }

    #[test]
    fn returns_multi_polygon_for_one_ring() {
        let result = read_polygon(vec![shell(0., 0., 10., 10.)]).unwrap();
        assert_eq!(1, result.0.len());
        assert_eq!(Coord { x: 0., y: 10. }, result.0[0].exterior().0[1]);
    }

    /// A clockwise comb: a body from y=0 to 10 with `teeth` teeth from x=2i to 2i+1 reaching up to y=20.
    fn comb(teeth: usize) -> Vec<Coord<f64>> {
        let mut points = new_coordinate_list(&[0., 0., 0., 10.]);
        for i in 0..teeth {
            let x = 2. * i as f64;
            points.extend(new_coordinate_list(&[x, 20., x + 1., 20., x + 1., 10., x + 2., 10.]));
        }
        points.extend(new_coordinate_list(&[2. * teeth as f64, 0., 0., 0.]));
        points
    }

    #[test]
    fn holes_in_and_between_teeth_of_large_non_convex_outer_ring() {
        let mut rings = vec![comb(50), shell(1000., 0., 1010., 10.)];
        for i in 0..50 {
            let x = 2. * i as f64;
            let y = (i % 5) as f64;
            // inside a tooth, then in the gap after it (outside the comb, but inside its envelope)
            rings.push(hole(x + 0.25, 12. + y, x + 0.75, 12.5 + y));
            rings.push(hole(x + 1.25, 15., x + 1.75, 15.5));
        }
        let result = read_polygon(rings).unwrap();
        // 50 holes inside the teeth go into the comb, and 50 in the gaps become polygons of their own after the other
        // outer ring
        assert_eq!(52, result.0.len());
        assert_eq!(50, result.0[0].interiors().len());
        assert!(result.0[1..].iter().all(|p| p.interiors().is_empty()));
        assert_eq!(hole(1.25, 15., 1.75, 15.5), result.0[2].exterior().0);
    }

    #[test]
    fn hole_level_with_outer_ring_vertices() {
        // a ray from each hole's first point (y=10) passes through every vertex where the teeth meet the body
        let mut rings = vec![comb(50), shell(1000., 0., 1010., 10.)];
        let mut expected_holes = 0;
        for i in (0..50).step_by(7) {
            rings.push(hole(2. * i as f64 + 0.25, 10., 2. * i as f64 + 0.75, 10.5));
            expected_holes += 1;
        }
        let result = read_polygon(rings).unwrap();
        assert_eq!(2, result.0.len());
        assert_eq!(expected_holes, result.0[0].interiors().len());
    }

    #[test]
    fn flat_outer_ring() {
        let flat = new_coordinate_list(&[0., 0., 5., 0., 10., 0., 0., 0.]);
        assert_reads(
            vec![
                new_polygon_from_coords(flat.clone()),
                polygon(shell(20., 0., 30., 10.), vec![hole(21., 1., 23., 3.)]),
            ],
            vec![flat.clone(), shell(20., 0., 30., 10.), hole(21., 1., 23., 3.)],
        );
        assert_reads(vec![polygon(flat.clone(), vec![hole(1., 1., 2., 2.)])], vec![flat, hole(1., 1., 2., 2.)]);
    }
}

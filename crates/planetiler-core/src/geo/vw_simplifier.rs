//! Port of `geo/VWSimplifier.java`: simplifies geometries using the Visvalingam Whyatt algorithm without any attempt
//! to repair geometries that become invalid due to simplification.

use geo_types::{Coord, Geometry};

use crate::geo::geometry_pipeline::GeometryPipeline;
use crate::geo::geometry_transformer::GeometryTransformer;

#[derive(Clone, Copy, Debug, Default)]
pub struct VWSimplifier {
    tolerance: f64,
    k: f64,
    keep_collapsed: bool,
}

/// Indexed binary min-heap ordered by `(value, id)` (equivalent to planetiler's `DoubleMinHeap` with an
/// `Integer::compare` tie-breaker) that supports updating the value of an element.
struct IndexedMinHeap {
    heap: Vec<usize>,
    pos: Vec<usize>,
    values: Vec<f64>,
}

const NOT_IN_HEAP: usize = usize::MAX;

impl IndexedMinHeap {
    /// Clears the heap and prepares it for ids `0..n`, reusing the existing allocations.
    fn reset(&mut self, n: usize) {
        self.heap.clear();
        self.heap.reserve(n);
        self.pos.clear();
        self.pos.resize(n, NOT_IN_HEAP);
        self.values.clear();
        self.values.resize(n, 0.0);
    }

    #[inline]
    fn less(&self, a: usize, b: usize) -> bool {
        let (va, vb) = (self.values[a], self.values[b]);
        va < vb || (va == vb && a < b)
    }

    fn push(&mut self, id: usize, value: f64) {
        self.values[id] = value;
        self.pos[id] = self.heap.len();
        self.heap.push(id);
        self.sift_up(self.heap.len() - 1);
    }

    fn update(&mut self, id: usize, value: f64) {
        let old = self.values[id];
        self.values[id] = value;
        let p = self.pos[id];
        if p == NOT_IN_HEAP {
            return;
        }
        if value < old {
            self.sift_up(p);
        } else {
            self.sift_down(p);
        }
    }

    fn poll(&mut self) -> Option<usize> {
        if self.heap.is_empty() {
            return None;
        }
        let top = self.heap.swap_remove(0);
        self.pos[top] = NOT_IN_HEAP;
        if !self.heap.is_empty() {
            self.pos[self.heap[0]] = 0;
            self.sift_down(0);
        }
        Some(top)
    }

    fn sift_up(&mut self, mut i: usize) {
        while i > 0 {
            let parent = (i - 1) / 2;
            if self.less(self.heap[i], self.heap[parent]) {
                self.swap(i, parent);
                i = parent;
            } else {
                break;
            }
        }
    }

    fn sift_down(&mut self, mut i: usize) {
        let n = self.heap.len();
        loop {
            let l = 2 * i + 1;
            let r = l + 1;
            let mut smallest = i;
            if l < n && self.less(self.heap[l], self.heap[smallest]) {
                smallest = l;
            }
            if r < n && self.less(self.heap[r], self.heap[smallest]) {
                smallest = r;
            }
            if smallest == i {
                break;
            }
            self.swap(i, smallest);
            i = smallest;
        }
    }

    #[inline]
    fn swap(&mut self, a: usize, b: usize) {
        self.heap.swap(a, b);
        self.pos[self.heap[a]] = a;
        self.pos[self.heap[b]] = b;
    }
}

const NONE: usize = usize::MAX;

/// Per-thread buffers reused between calls so simplifying small geometries doesn't allocate.
struct Scratch {
    prev: Vec<usize>,
    next: Vec<usize>,
    areas: Vec<f64>,
    heap: IndexedMinHeap,
}

impl Scratch {
    fn new() -> Self {
        Scratch {
            prev: Vec::new(),
            next: Vec::new(),
            areas: Vec::new(),
            heap: IndexedMinHeap { heap: Vec::new(), pos: Vec::new(), values: Vec::new() },
        }
    }
}

thread_local! {
    static SCRATCH: std::cell::RefCell<Scratch> = std::cell::RefCell::new(Scratch::new());
}

/// Area of the triangle formed by 3 points.
#[inline]
pub fn triangle_area(ax: f64, ay: f64, bx: f64, by: f64, cx: f64, cy: f64) -> f64 {
    (((ay - cy) * (bx - cx) + (by - cy) * (cx - ax)) / 2.0).abs()
}

#[inline]
fn cos(ax: f64, ay: f64, bx: f64, by: f64, cx: f64, cy: f64) -> f64 {
    let den = (bx - ax).hypot(by - ay) * (cx - bx).hypot(cy - by);
    if den > 0.0 { (((ax - bx) * (cx - bx) + (ay - by) * (cy - by)) / den).clamp(-1.0, 1.0) } else { 0.0 }
}

impl VWSimplifier {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the minimum effective triangle area created by 3 consecutive vertices in order to retain that vertex.
    pub fn set_tolerance(mut self, tolerance: f64) -> Self {
        self.tolerance = tolerance;
        self
    }

    /// Set to `true` to keep polygons with area smaller than `tolerance`.
    pub fn set_keep_collapsed(mut self, keep_collapsed: bool) -> Self {
        self.keep_collapsed = keep_collapsed;
        self
    }

    /// Apply a penalty from `k=0` to `k=1` to drop more sharp corners from the resulting geometry (`0.7` is
    /// recommended).
    pub fn set_weight(mut self, k: f64) -> Self {
        self.k = k;
        self
    }

    #[inline]
    fn weighted_area(&self, ax: f64, ay: f64, bx: f64, by: f64, cx: f64, cy: f64) -> f64 {
        let area = triangle_area(ax, ay, bx, by, cx, cy);
        if self.k == 0.0 { area } else { area * ((-cos(ax, ay, bx, by, cx, cy)) * self.k + 1.0) }
    }

    /// Simplify a coordinate sequence; `area` is true for polygon rings.
    pub fn simplify_coordinates(&self, coords: &[Coord<f64>], area: bool) -> Vec<Coord<f64>> {
        let min_points = if self.keep_collapsed && area { 4 } else { 2 };
        let num = coords.len();
        if num <= min_points {
            return coords.to_vec();
        }
        SCRATCH.with(|scratch| match scratch.try_borrow_mut() {
            Ok(mut scratch) => self.simplify_with(coords, min_points, &mut scratch),
            Err(_) => self.simplify_with(coords, min_points, &mut Scratch::new()),
        })
    }

    fn simplify_with(&self, coords: &[Coord<f64>], min_points: usize, scratch: &mut Scratch) -> Vec<Coord<f64>> {
        let num = coords.len();
        let Scratch { prev, next, areas, heap } = scratch;
        prev.clear();
        prev.resize(num, NONE);
        next.clear();
        next.resize(num, NONE);
        areas.clear();
        areas.resize(num, 0.0);
        heap.reset(num);
        let (prev, next, areas) = (&mut prev[..], &mut next[..], &mut areas[..]);

        let update_area = |i: usize, prev: &[usize], next: &[usize]| -> f64 {
            let (p, n) = (prev[i], next[i]);
            if p == NONE || n == NONE {
                f64::INFINITY
            } else {
                let (a, b, c) = (coords[p], coords[i], coords[n]);
                self.weighted_area(a.x, a.y, b.x, b.y, c.x, c.y)
            }
        };

        for i in 1..num {
            prev[i] = i - 1;
            next[i - 1] = i;
            areas[i - 1] = update_area(i - 1, prev, next);
            heap.push(i - 1, areas[i - 1]);
        }
        areas[num - 1] = update_area(num - 1, prev, next);
        heap.push(num - 1, areas[num - 1]);

        let mut left = num;
        while let Some(id) = heap.poll() {
            if areas[id] > self.tolerance || left <= min_points {
                break;
            }
            let (p, n) = (prev[id], next[id]);
            if p != NONE {
                next[p] = n;
            }
            if n != NONE {
                prev[n] = p;
            }
            left -= 1;
            if p != NONE {
                areas[p] = update_area(p, prev, next);
                heap.update(p, areas[p]);
            }
            if n != NONE {
                areas[n] = update_area(n, prev, next);
                heap.update(n, areas[n]);
            }
        }
        let mut result = Vec::with_capacity(left);
        let mut i = 0;
        while i != NONE {
            result.push(coords[i]);
            i = next[i];
        }
        result
    }
}

impl GeometryTransformer for VWSimplifier {
    fn transform_coordinates(&mut self, coords: &[Coord<f64>], is_ring: bool) -> Option<Vec<Coord<f64>>> {
        Some(self.simplify_coordinates(coords, is_ring))
    }

    fn remove_degenerate_rings(&self) -> bool {
        true
    }
}

impl GeometryPipeline for VWSimplifier {
    fn apply(&self, input: &Geometry<f64>) -> Geometry<f64> {
        let mut copy = *self;
        copy.transform(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::test_utils::*;
    use geo::Rotate;
    use geo_types::{LineString, Point, Polygon};

    const ROTATIONS: [f64; 5] = [0.0, 45.0, 90.0, 180.0, 270.0];

    /// JTS `Geometry.reverse()`
    fn reverse(geom: &Geometry<f64>) -> Geometry<f64> {
        let rev = |l: &LineString<f64>| LineString(l.0.iter().rev().copied().collect());
        match geom {
            Geometry::LineString(l) => Geometry::LineString(rev(l)),
            Geometry::Polygon(p) => {
                Geometry::Polygon(Polygon::new(rev(p.exterior()), p.interiors().iter().map(rev).collect()))
            }
            other => other.clone(),
        }
    }

    fn test_simplify(input: Geometry<f64>, expected: Geometry<f64>, amount: f64) {
        for rotation in ROTATIONS {
            let origin = Point::new(0.0, 0.0);
            let rotate = |g: &Geometry<f64>| g.rotate_around_point(rotation, origin);
            let mut simplifier = VWSimplifier::new().set_tolerance(amount).set_weight(0.0);
            assert_same_normalized_feature(&rotate(&expected), &simplifier.transform(&rotate(&input)));
            assert_same_normalized_feature(
                &rotate(&reverse(&expected)),
                &simplifier.transform(&rotate(&reverse(&input))),
            );
        }
    }

    #[test]
    fn simplify_point() {
        test_simplify(new_point(1.0, 1.0).into(), new_point(1.0, 1.0).into(), 1.0);
    }

    #[test]
    fn simplify_2_points() {
        test_simplify(
            new_line_string(&[0.0, 0.0, 10.0, 10.0]).into(),
            new_line_string(&[0.0, 0.0, 10.0, 10.0]).into(),
            1.0,
        );
    }

    #[test]
    fn remove_a_point() {
        test_simplify(
            new_line_string(&[0.0, 0.0, 5.0, 0.9, 10.0, 0.0]).into(),
            new_line_string(&[0.0, 0.0, 10.0, 0.0]).into(),
            5.0,
        );
    }

    #[test]
    fn keep_a_point() {
        test_simplify(
            new_line_string(&[0.0, 0.0, 5.0, 1.1, 10.0, 0.0]).into(),
            new_line_string(&[0.0, 0.0, 5.0, 1.1, 10.0, 0.0]).into(),
            5.0,
        );
    }

    #[test]
    fn polygon_leave_a_point_when_area_less_than_tolerance() {
        let input: Geometry<f64> = new_polygon(&[0.0, 0.0, 10.0, 10.0, 9.0, 10.0, 0.0, 8.0, 0.0, 0.0]).into();
        let collapsed = VWSimplifier::new().set_tolerance(200.0).set_weight(0.0).transform(&input);
        assert_same_normalized_feature(&empty_geometry(), &collapsed);
        assert_same_normalized_feature(
            &new_polygon(&[0.0, 0.0, 0.0, 8.0, 10.0, 10.0, 0.0, 0.0]).into(),
            &VWSimplifier::new().set_tolerance(200.0).set_keep_collapsed(true).set_weight(0.0).transform(&input),
        );
    }

    #[test]
    fn line() {
        let input: Geometry<f64> = new_line_string(&[0.0, 0.0, 1.0, 0.1, 2.0, 0.0, 3.0, 0.1]).into();
        test_simplify(input.clone(), input.clone(), 0.09);
        test_simplify(input, new_line_string(&[0.0, 0.0, 3.0, 0.1]).into(), 0.11);
    }
}

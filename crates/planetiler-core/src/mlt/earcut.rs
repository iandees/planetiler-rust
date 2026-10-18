//! Port of [earcut4j](https://github.com/maplibre/earcut4j) 3.0.0 (ISC license), the polygon triangulation used by
//! MLT 0.0.10 for pre-tessellated polygons. Nodes live in an arena and link to each other by index; the algorithm is
//! otherwise a line-by-line port so the triangles match Java exactly.

const NIL: usize = usize::MAX;

#[derive(Clone, Debug)]
struct Node {
    i: i32,
    x: f64,
    y: f64,
    z: f64,
    steiner: bool,
    prev: usize,
    next: usize,
    prev_z: usize,
    next_z: usize,
}

struct Earcut {
    nodes: Vec<Node>,
    triangles: Vec<i32>,
}

/// Triangulates a polygon given as flat `[x0, y0, x1, y1, ...]` coordinates (2 dimensions) with holes starting at the
/// vertex indices in `hole_indices`. Returns vertex indices, 3 per triangle.
pub fn earcut(data: &[f64], hole_indices: &[i32]) -> Vec<i32> {
    let dim = 2usize;
    let has_holes = !hole_indices.is_empty();
    let outer_len = if has_holes { hole_indices[0] as usize * dim } else { data.len() };
    let mut e = Earcut { nodes: Vec::with_capacity(data.len() / 2 * 3 / 2 + 4), triangles: Vec::new() };
    let mut outer_node = e.linked_list(data, 0, outer_len, dim, true);
    if outer_node == NIL || e.nodes[outer_node].next == e.nodes[outer_node].prev {
        return e.triangles;
    }
    let mut min_x = 0.0;
    let mut min_y = 0.0;
    let mut inv_size = f64::from_bits(1); // Double.MIN_VALUE
    if has_holes {
        outer_node = e.eliminate_holes(data, hole_indices, outer_node, dim);
    }
    // if the shape is not too simple, we'll use z-order curve hash later; calculate polygon bbox
    if data.len() > 80 * dim {
        min_x = data[0];
        let mut max_x = data[0];
        min_y = data[1];
        let mut max_y = data[1];
        let mut i = dim;
        while i < outer_len {
            let x = data[i];
            let y = data[i + 1];
            if x < min_x {
                min_x = x;
            }
            if y < min_y {
                min_y = y;
            }
            if x > max_x {
                max_x = x;
            }
            if y > max_y {
                max_y = y;
            }
            i += dim;
        }
        // minX, minY and size are later used to transform coords into integers for z-order calculation
        inv_size = (max_x - min_x).max(max_y - min_y);
        inv_size = if inv_size != 0.0 { 32767.0 / inv_size } else { 0.0 };
    }
    e.earcut_linked(outer_node, min_x, min_y, inv_size, 0);
    e.triangles
}

const MIN_VALUE: f64 = f64::from_bits(1);

/// Java `Math.max(double, double)` for the non-NaN inputs here.
#[inline]
fn jmax(a: f64, b: f64) -> f64 {
    if a >= b { a } else { b }
}

#[inline]
fn jmin(a: f64, b: f64) -> f64 {
    if a <= b { a } else { b }
}

#[inline]
fn sign(num: f64) -> f64 {
    if num > 0.0 {
        1.0
    } else if num < 0.0 {
        -1.0
    } else {
        0.0
    }
}

/// z-order of a point given coords and inverse of the longer side of data bbox
fn z_order(x: f64, y: f64, min_x: f64, min_y: f64, inv_size: f64) -> f64 {
    // coords are transformed into non-negative 15-bit integer range
    let mut lx = ((x - min_x) * inv_size) as i32;
    let mut ly = ((y - min_y) * inv_size) as i32;

    lx = (lx | (lx << 8)) & 0x00FF00FF;
    lx = (lx | (lx << 4)) & 0x0F0F0F0F;
    lx = (lx | (lx << 2)) & 0x33333333;
    lx = (lx | (lx << 1)) & 0x55555555;

    ly = (ly | (ly << 8)) & 0x00FF00FF;
    ly = (ly | (ly << 4)) & 0x0F0F0F0F;
    ly = (ly | (ly << 2)) & 0x33333333;
    ly = (ly | (ly << 1)) & 0x55555555;

    (lx | (ly << 1)) as f64
}

#[inline]
#[allow(clippy::too_many_arguments)]
fn point_in_triangle(ax: f64, ay: f64, bx: f64, by: f64, cx: f64, cy: f64, px: f64, py: f64) -> bool {
    (cx - px) * (ay - py) >= (ax - px) * (cy - py)
        && (ax - px) * (by - py) >= (bx - px) * (ay - py)
        && (bx - px) * (cy - py) >= (cx - px) * (by - py)
}

#[inline]
#[allow(clippy::too_many_arguments)]
fn point_in_triangle_except_first(ax: f64, ay: f64, bx: f64, by: f64, cx: f64, cy: f64, px: f64, py: f64) -> bool {
    !(ax == px && ay == py) && point_in_triangle(ax, ay, bx, by, cx, cy, px, py)
}

fn signed_area(data: &[f64], start: usize, end: usize, dim: usize) -> f64 {
    let mut sum = 0.0;
    let mut j = end - dim;
    let mut i = start;
    while i < end {
        sum += (data[j] - data[i]) * (data[i + 1] + data[j + 1]);
        j = i;
        i += dim;
    }
    sum
}

impl Earcut {
    #[inline]
    fn n(&self, i: usize) -> &Node {
        &self.nodes[i]
    }

    #[inline]
    fn next(&self, i: usize) -> usize {
        self.nodes[i].next
    }

    #[inline]
    fn prev(&self, i: usize) -> usize {
        self.nodes[i].prev
    }

    #[inline]
    fn equals(&self, a: usize, b: usize) -> bool {
        let (p1, p2) = (self.n(a), self.n(b));
        p1.x == p2.x && p1.y == p2.y
    }

    #[inline]
    fn area(&self, p: usize, q: usize, r: usize) -> f64 {
        let (p, q, r) = (self.n(p), self.n(q), self.n(r));
        (q.y - p.y) * (r.x - q.x) - (q.x - p.x) * (r.y - q.y)
    }

    fn new_node(&mut self, i: i32, x: f64, y: f64) -> usize {
        self.nodes.push(Node { i, x, y, z: MIN_VALUE, steiner: false, prev: NIL, next: NIL, prev_z: NIL, next_z: NIL });
        self.nodes.len() - 1
    }

    fn insert_node(&mut self, i: i32, x: f64, y: f64, last: usize) -> usize {
        let p = self.new_node(i, x, y);
        if last == NIL {
            self.nodes[p].prev = p;
            self.nodes[p].next = p;
        } else {
            let last_next = self.next(last);
            self.nodes[p].next = last_next;
            self.nodes[p].prev = last;
            self.nodes[last_next].prev = p;
            self.nodes[last].next = p;
        }
        p
    }

    fn remove_node(&mut self, p: usize) {
        let (prev, next, prev_z, next_z) = {
            let n = self.n(p);
            (n.prev, n.next, n.prev_z, n.next_z)
        };
        self.nodes[next].prev = prev;
        self.nodes[prev].next = next;
        if prev_z != NIL {
            self.nodes[prev_z].next_z = next_z;
        }
        if next_z != NIL {
            self.nodes[next_z].prev_z = prev_z;
        }
    }

    fn linked_list(&mut self, data: &[f64], start: usize, end: usize, dim: usize, clockwise: bool) -> usize {
        let mut last = NIL;
        if clockwise == (signed_area(data, start, end, dim) > 0.0) {
            let mut i = start;
            while i < end {
                last = self.insert_node((i / dim) as i32, data[i], data[i + 1], last);
                i += dim;
            }
        } else {
            let mut i = end as isize - dim as isize;
            while i >= start as isize {
                let iu = i as usize;
                last = self.insert_node((iu / dim) as i32, data[iu], data[iu + 1], last);
                i -= dim as isize;
            }
        }
        if last != NIL && self.equals(last, self.next(last)) {
            self.remove_node(last);
            last = self.next(last);
        }
        last
    }

    fn earcut_linked(&mut self, ear: usize, min_x: f64, min_y: f64, inv_size: f64, pass: i32) {
        if ear == NIL {
            return;
        }
        let mut ear = ear;
        // interlink polygon nodes in z-order
        if pass == 0 && inv_size != MIN_VALUE {
            self.index_curve(ear, min_x, min_y, inv_size);
        }
        let mut stop = ear;
        // iterate through ears, slicing them one by one
        while self.prev(ear) != self.next(ear) {
            let prev = self.prev(ear);
            let next = self.next(ear);
            let is_ear =
                if inv_size != MIN_VALUE { self.is_ear_hashed(ear, min_x, min_y, inv_size) } else { self.is_ear(ear) };
            if is_ear {
                // cut off the triangle
                self.triangles.push(self.n(prev).i);
                self.triangles.push(self.n(ear).i);
                self.triangles.push(self.n(next).i);
                self.remove_node(ear);
                // skipping the next vertice leads to less sliver triangles
                ear = self.next(next);
                stop = self.next(next);
                continue;
            }
            ear = next;
            // if we looped through the whole remaining polygon and can't find any more ears
            if ear == stop {
                if pass == 0 {
                    // try filtering points and slicing again
                    let filtered = self.filter_points(ear, NIL);
                    self.earcut_linked(filtered, min_x, min_y, inv_size, 1);
                } else if pass == 1 {
                    // if this didn't work, try curing all small self-intersections locally
                    let filtered = self.filter_points(ear, NIL);
                    let cured = self.cure_local_intersections(filtered);
                    self.earcut_linked(cured, min_x, min_y, inv_size, 2);
                } else if pass == 2 {
                    // as a last resort, try splitting the remaining polygon into two
                    self.split_earcut(ear, min_x, min_y, inv_size);
                }
                break;
            }
        }
    }

    fn split_earcut(&mut self, start: usize, min_x: f64, min_y: f64, size: f64) {
        // look for a valid diagonal that divides the polygon into two
        let mut a = start;
        loop {
            let mut b = self.next(self.next(a));
            while b != self.prev(a) {
                if self.n(a).i != self.n(b).i && self.is_valid_diagonal(a, b) {
                    // split the polygon in two by the diagonal
                    let mut c = self.split_polygon(a, b);
                    // filter colinear points around the cuts
                    let a_next = self.next(a);
                    let a2 = self.filter_points(a, a_next);
                    let c_next = self.next(c);
                    c = self.filter_points(c, c_next);
                    // run earcut on each half
                    self.earcut_linked(a2, min_x, min_y, size, 0);
                    self.earcut_linked(c, min_x, min_y, size, 0);
                    return;
                }
                b = self.next(b);
            }
            a = self.next(a);
            if a == start {
                break;
            }
        }
    }

    fn is_valid_diagonal(&self, a: usize, b: usize) -> bool {
        let (an, ap) = (self.next(a), self.prev(a));
        let (bn, bp) = (self.next(b), self.prev(b));
        self.n(an).i != self.n(b).i
            && self.n(ap).i != self.n(b).i
            && !self.intersects_polygon(a, b)
            && ((self.locally_inside(a, b)
                && self.locally_inside(b, a)
                && self.middle_inside(a, b)
                && (self.area(ap, a, bp) != 0.0 || self.area(a, bp, b) != 0.0))
                || (self.equals(a, b) && self.area(ap, a, an) > 0.0 && self.area(bp, b, bn) > 0.0))
    }

    fn middle_inside(&self, a: usize, b: usize) -> bool {
        let mut p = a;
        let mut inside = false;
        let px = (self.n(a).x + self.n(b).x) / 2.0;
        let py = (self.n(a).y + self.n(b).y) / 2.0;
        loop {
            let pn = self.n(p);
            let pnext = self.n(pn.next);
            if ((pn.y > py) != (pnext.y > py))
                && pnext.y != pn.y
                && (px < (pnext.x - pn.x) * (py - pn.y) / (pnext.y - pn.y) + pn.x)
            {
                inside = !inside;
            }
            p = pn.next;
            if p == a {
                break;
            }
        }
        inside
    }

    fn intersects_polygon(&self, a: usize, b: usize) -> bool {
        let (ai, bi) = (self.n(a).i, self.n(b).i);
        let mut p = a;
        loop {
            let pn = self.next(p);
            let (pi, pni) = (self.n(p).i, self.n(pn).i);
            if pi != ai && pni != ai && pi != bi && pni != bi && self.intersects(p, pn, a, b) {
                return true;
            }
            p = pn;
            if p == a {
                break;
            }
        }
        false
    }

    fn intersects(&self, p1: usize, q1: usize, p2: usize, q2: usize) -> bool {
        let o1 = sign(self.area(p1, q1, p2));
        let o2 = sign(self.area(p1, q1, q2));
        let o3 = sign(self.area(p2, q2, p1));
        let o4 = sign(self.area(p2, q2, q1));
        if o1 != o2 && o3 != o4 {
            return true; // general case
        }
        if o1 == 0.0 && self.on_segment(p1, p2, q1) {
            return true; // p1, q1 and p2 are collinear and p2 lies on p1q1
        }
        if o2 == 0.0 && self.on_segment(p1, q2, q1) {
            return true; // p1, q1 and q2 are collinear and q2 lies on p1q1
        }
        if o3 == 0.0 && self.on_segment(p2, p1, q2) {
            return true; // p2, q2 and p1 are collinear and p1 lies on p2q2
        }
        if o4 == 0.0 && self.on_segment(p2, q1, q2) {
            return true; // p2, q2 and q1 are collinear and q1 lies on p2q2
        }
        false
    }

    // for collinear points p, q, r, check if point q lies on segment pr
    fn on_segment(&self, p: usize, q: usize, r: usize) -> bool {
        let (p, q, r) = (self.n(p), self.n(q), self.n(r));
        q.x <= jmax(p.x, r.x) && q.x >= jmin(p.x, r.x) && q.y <= jmax(p.y, r.y) && q.y >= jmin(p.y, r.y)
    }

    fn cure_local_intersections(&mut self, start: usize) -> usize {
        let mut start = start;
        let mut p = start;
        loop {
            let a = self.prev(p);
            let b = self.next(self.next(p));
            if !self.equals(a, b)
                && self.intersects(a, p, self.next(p), b)
                && self.locally_inside(a, b)
                && self.locally_inside(b, a)
            {
                self.triangles.push(self.n(a).i);
                self.triangles.push(self.n(p).i);
                self.triangles.push(self.n(b).i);
                // remove two nodes involved
                self.remove_node(p);
                let pn = self.next(p);
                self.remove_node(pn);
                p = b;
                start = b;
            }
            p = self.next(p);
            if p == start {
                break;
            }
        }
        self.filter_points(p, NIL)
    }

    fn is_ear(&self, ear: usize) -> bool {
        let (a, b, c) = (self.prev(ear), ear, self.next(ear));
        if self.area(a, b, c) >= 0.0 {
            return false; // reflex, can't be an ear
        }
        let (an, bn, cn) = (self.n(a), self.n(b), self.n(c));
        // now make sure we don't have other points inside the potential ear
        let mut p = cn.next;
        while p != a {
            let pn = self.n(p);
            if point_in_triangle_except_first(an.x, an.y, bn.x, bn.y, cn.x, cn.y, pn.x, pn.y)
                && self.area(pn.prev, p, pn.next) >= 0.0
            {
                return false;
            }
            p = pn.next;
        }
        true
    }

    fn is_ear_hashed(&self, ear: usize, min_x: f64, min_y: f64, inv_size: f64) -> bool {
        let (a, b, c) = (self.prev(ear), ear, self.next(ear));
        if self.area(a, b, c) >= 0.0 {
            return false; // reflex, can't be an ear
        }
        let (an, bn, cn) = (self.n(a), self.n(b), self.n(c));
        // triangle bbox; min & max are calculated like this for speed
        let min_tx = if an.x < bn.x {
            if an.x < cn.x { an.x } else { cn.x }
        } else if bn.x < cn.x {
            bn.x
        } else {
            cn.x
        };
        let min_ty = if an.y < bn.y {
            if an.y < cn.y { an.y } else { cn.y }
        } else if bn.y < cn.y {
            bn.y
        } else {
            cn.y
        };
        let max_tx = if an.x > bn.x {
            if an.x > cn.x { an.x } else { cn.x }
        } else if bn.x > cn.x {
            bn.x
        } else {
            cn.x
        };
        let max_ty = if an.y > bn.y {
            if an.y > cn.y { an.y } else { cn.y }
        } else if bn.y > cn.y {
            bn.y
        } else {
            cn.y
        };

        // z-order range for the current triangle bbox;
        let min_z = z_order(min_tx, min_ty, min_x, min_y, inv_size);
        let max_z = z_order(max_tx, max_ty, min_x, min_y, inv_size);

        let check = |p: usize| -> bool {
            let pn = self.n(p);
            p != a
                && p != c
                && point_in_triangle_except_first(an.x, an.y, bn.x, bn.y, cn.x, cn.y, pn.x, pn.y)
                && self.area(pn.prev, p, pn.next) >= 0.0
        };

        // first look for points inside the triangle in increasing z-order
        let mut p = self.n(ear).prev_z;
        let mut n = self.n(ear).next_z;
        while p != NIL && self.n(p).z >= min_z && n != NIL && self.n(n).z <= max_z {
            if check(p) {
                return false;
            }
            p = self.n(p).prev_z;
            if check(n) {
                return false;
            }
            n = self.n(n).next_z;
        }
        // look for remaining points in decreasing z-order
        while p != NIL && self.n(p).z >= min_z {
            if check(p) {
                return false;
            }
            p = self.n(p).prev_z;
        }
        // look for remaining points in increasing z-order
        while n != NIL && self.n(n).z <= max_z {
            if check(n) {
                return false;
            }
            n = self.n(n).next_z;
        }
        true
    }

    fn index_curve(&mut self, start: usize, min_x: f64, min_y: f64, inv_size: f64) {
        let mut p = start;
        loop {
            if self.nodes[p].z == MIN_VALUE {
                self.nodes[p].z = z_order(self.nodes[p].x, self.nodes[p].y, min_x, min_y, inv_size);
            }
            self.nodes[p].prev_z = self.nodes[p].prev;
            self.nodes[p].next_z = self.nodes[p].next;
            p = self.nodes[p].next;
            if p == start {
                break;
            }
        }
        let pz = self.nodes[p].prev_z;
        self.nodes[pz].next_z = NIL;
        self.nodes[p].prev_z = NIL;
        self.sort_linked(p);
    }

    fn sort_linked(&mut self, list: usize) -> usize {
        let mut list = list;
        let mut in_size = 1;
        loop {
            let mut p = list;
            list = NIL;
            let mut tail = NIL;
            let mut num_merges = 0;
            while p != NIL {
                num_merges += 1;
                let mut q = p;
                let mut p_size = 0;
                for _ in 0..in_size {
                    p_size += 1;
                    q = self.nodes[q].next_z;
                    if q == NIL {
                        break;
                    }
                }
                let mut q_size = in_size;
                while p_size > 0 || (q_size > 0 && q != NIL) {
                    let e;
                    if p_size == 0 {
                        e = q;
                        q = self.nodes[q].next_z;
                        q_size -= 1;
                    } else if q_size == 0 || q == NIL || self.nodes[p].z <= self.nodes[q].z {
                        e = p;
                        p = self.nodes[p].next_z;
                        p_size -= 1;
                    } else {
                        e = q;
                        q = self.nodes[q].next_z;
                        q_size -= 1;
                    }
                    if tail != NIL {
                        self.nodes[tail].next_z = e;
                    } else {
                        list = e;
                    }
                    self.nodes[e].prev_z = tail;
                    tail = e;
                }
                p = q;
            }
            self.nodes[tail].next_z = NIL;
            in_size *= 2;
            if num_merges <= 1 {
                break;
            }
        }
        list
    }

    fn eliminate_holes(&mut self, data: &[f64], hole_indices: &[i32], outer_node: usize, dim: usize) -> usize {
        let mut queue = Vec::with_capacity(hole_indices.len());
        let len = hole_indices.len();
        for i in 0..len {
            let start = hole_indices[i] as usize * dim;
            let end = if i < len - 1 { hole_indices[i + 1] as usize * dim } else { data.len() };
            let list = self.linked_list(data, start, end, dim, false);
            if list == self.next(list) {
                self.nodes[list].steiner = true;
            }
            queue.push(self.get_leftmost(list));
        }
        queue.sort_by(|&o1, &o2| {
            let (n1, n2) = (self.n(o1), self.n(o2));
            let mut result = n1.x - n2.x;
            // when the left-most point of 2 holes meet at a vertex, sort the holes counterclockwise so that when we
            // find the bridge to the outer shell is always the point that they meet at.
            if result == 0.0 {
                result = n1.y - n2.y;
                if result == 0.0 {
                    let (n1n, n2n) = (self.n(n1.next), self.n(n2.next));
                    let a_slope = (n1n.y - n1.y) / (n1n.x - n1.x);
                    let b_slope = (n2n.y - n2.y) / (n2n.x - n2.x);
                    result = a_slope - b_slope;
                }
            }
            if result > 0.0 {
                std::cmp::Ordering::Greater
            } else if result < 0.0 {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        });
        let mut outer_node = outer_node;
        for node in queue {
            outer_node = self.eliminate_hole(node, outer_node);
        }
        outer_node
    }

    fn filter_points(&mut self, start: usize, end: usize) -> usize {
        if start == NIL {
            return start;
        }
        let mut end = if end == NIL { start } else { end };
        let mut p = start;
        loop {
            let mut again = false;
            let (pp, pn) = (self.prev(p), self.next(p));
            if !self.nodes[p].steiner && (self.equals(p, pn) || self.area(pp, p, pn) == 0.0) {
                self.remove_node(p);
                p = self.prev(p);
                end = p;
                if p == self.next(p) {
                    break;
                }
                again = true;
            } else {
                p = self.next(p);
            }
            if !(again || p != end) {
                break;
            }
        }
        end
    }

    fn eliminate_hole(&mut self, hole: usize, outer_node: usize) -> usize {
        let bridge = self.find_hole_bridge(hole, outer_node);
        if bridge != NIL {
            let b = self.split_polygon(bridge, hole);
            // filter collinear points around the cuts
            let b_next = self.next(b);
            self.filter_points(b, b_next);
            let bridge_next = self.next(bridge);
            return self.filter_points(bridge, bridge_next);
        }
        outer_node
    }

    fn split_polygon(&mut self, a: usize, b: usize) -> usize {
        let (ai, ax, ay) = (self.n(a).i, self.n(a).x, self.n(a).y);
        let (bi, bx, by) = (self.n(b).i, self.n(b).x, self.n(b).y);
        let a2 = self.new_node(ai, ax, ay);
        let b2 = self.new_node(bi, bx, by);
        let an = self.next(a);
        let bp = self.prev(b);

        self.nodes[a].next = b;
        self.nodes[b].prev = a;

        self.nodes[a2].next = an;
        self.nodes[an].prev = a2;

        self.nodes[b2].next = a2;
        self.nodes[a2].prev = b2;

        self.nodes[bp].next = b2;
        self.nodes[b2].prev = bp;

        b2
    }

    /// David Eberly's algorithm for finding a bridge between hole and outer polygon
    fn find_hole_bridge(&self, hole: usize, outer_node: usize) -> usize {
        let mut p = outer_node;
        let hx = self.n(hole).x;
        let hy = self.n(hole).y;
        let mut qx = -f64::MAX;
        let mut m = NIL;

        // find a segment intersected by a ray from the hole's leftmost point to the left; segment's endpoint with
        // lesser x will be potential connection point unless they intersect at a vertex, then choose the vertex
        if self.equals(hole, p) {
            return p;
        }
        loop {
            let pn = self.next(p);
            let (pnode, pnext) = (self.n(p), self.n(pn));
            if self.equals(hole, pn) {
                return pn;
            } else if hy <= pnode.y && hy >= pnext.y && pnext.y != pnode.y {
                let x = pnode.x + (hy - pnode.y) * (pnext.x - pnode.x) / (pnext.y - pnode.y);
                if x <= hx && x > qx {
                    qx = x;
                    m = if pnode.x < pnext.x { p } else { pn };
                    if x == hx {
                        return m; // hole touches outer segment; pick leftmost endpoint
                    }
                }
            }
            p = pn;
            if p == outer_node {
                break;
            }
        }
        if m == NIL {
            return NIL;
        }
        if hx == qx {
            return m; // hole touches outer segment; pick leftmost endpoint
        }

        // look for points inside the triangle of hole point, segment intersection and endpoint; if there are no
        // points found, we have a valid connection; otherwise choose the point of the minimum angle with the ray as
        // connection point
        let stop = m;
        let mx = self.n(m).x;
        let my = self.n(m).y;
        let mut tan_min = f64::MAX;
        p = m;
        loop {
            let (px, py) = (self.n(p).x, self.n(p).y);
            if hx >= px
                && px >= mx
                && hx != px
                && point_in_triangle(
                    if hy < my { hx } else { qx },
                    hy,
                    mx,
                    my,
                    if hy < my { qx } else { hx },
                    hy,
                    px,
                    py,
                )
            {
                let tan = (hy - py).abs() / (hx - px); // tangential
                let mnode = self.n(m);
                if self.locally_inside(p, hole)
                    && (tan < tan_min
                        || (tan == tan_min && (px > mnode.x || (px == mnode.x && self.sector_contains_sector(m, p)))))
                {
                    m = p;
                    tan_min = tan;
                }
            }
            p = self.next(p);
            if p == stop {
                break;
            }
        }
        m
    }

    fn locally_inside(&self, a: usize, b: usize) -> bool {
        let (ap, an) = (self.prev(a), self.next(a));
        if self.area(ap, a, an) < 0.0 {
            self.area(a, b, an) >= 0.0 && self.area(a, ap, b) >= 0.0
        } else {
            self.area(a, b, ap) < 0.0 || self.area(a, an, b) < 0.0
        }
    }

    // whether sector in vertex m contains sector in vertex p in the same coordinates
    fn sector_contains_sector(&self, m: usize, p: usize) -> bool {
        self.area(self.prev(m), m, self.prev(p)) < 0.0 && self.area(self.next(p), m, self.next(m)) < 0.0
    }

    fn get_leftmost(&self, start: usize) -> usize {
        let mut p = start;
        let mut leftmost = start;
        loop {
            let (pn, ln) = (self.n(p), self.n(leftmost));
            if pn.x < ln.x || (pn.x == ln.x && pn.y < ln.y) {
                leftmost = p;
            }
            p = pn.next;
            if p == start {
                break;
            }
        }
        leftmost
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn square() {
        let t = earcut(&[0.0, 0.0, 10.0, 0.0, 10.0, 10.0, 0.0, 10.0], &[]);
        assert_eq!(6, t.len());
        // matches earcut.js
        assert_eq!(vec![2, 3, 0, 0, 1, 2], t);
    }

    #[test]
    fn square_with_hole() {
        let data = [0.0, 0.0, 10.0, 0.0, 10.0, 10.0, 0.0, 10.0, 2.0, 2.0, 2.0, 8.0, 8.0, 8.0, 8.0, 2.0];
        let t = earcut(&data, &[4]);
        assert_eq!(8 * 3, t.len());
    }

    #[test]
    fn large_polygon_uses_hashing() {
        let mut data = Vec::new();
        for i in 0..100 {
            let a = i as f64 / 100.0 * std::f64::consts::TAU;
            let r = if i % 2 == 0 { 100.0 } else { 60.0 };
            data.push(r * a.cos());
            data.push(r * a.sin());
        }
        let t = earcut(&data, &[]);
        assert_eq!(98 * 3, t.len());
    }
}

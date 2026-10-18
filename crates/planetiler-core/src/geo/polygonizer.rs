//! Port of JTS `Polygonizer` (`org.locationtech.jts.operation.polygonize`: `Polygonizer`, `PolygonizeGraph`,
//! `PolygonizeDirectedEdge`, `EdgeRing`, `HoleAssigner` and the parts of `planargraph` they use).
//!
//! Polygonizes a set of line strings which must be correctly noded (they may only touch at their endpoints). The
//! result is the polygons formed by the faces of the planar graph, plus the edges that could not be used:
//!
//! - **dangles**: edges with one or both ends not incident on another edge
//! - **cut edges**: edges connected at both ends that do not form part of a polygon (both sides are the same face)
//! - **invalid ring lines**: rings that are not valid polygon shells (e.g. self-intersecting because the input was
//!   not noded)
//!
//! With `extract_only_polygonal` only shells whose faces alternate with "outer" faces are returned so that the result
//! is a valid polygonal geometry (no two polygons share an edge).
//!
//! Differences from Java: the graph keeps directed edges in insertion order (JTS uses a `HashSet`, so the order rings
//! are discovered in, and their start points, vary between JVM runs); nodes are visited in coordinate order like the
//! JTS `TreeMap`. Where JTS would fail an assertion or loop forever on badly-noded input, this stops the ring walk.

use std::cmp::Ordering;

use geo::kernels::{Kernel, Orientation, RobustKernel};
use geo_types::{Coord, Geometry, GeometryCollection, LineString, MultiPolygon, Polygon};
use hashbrown::HashMap;

use crate::geo::is_valid::{Location, is_valid_polygons, locate_in_ring};

const NONE: usize = usize::MAX;

#[derive(Debug)]
struct Node {
    coord: Coord<f64>,
    /// outgoing directed edges, sorted counter-clockwise by angle once the graph is built
    out: Vec<usize>,
}

#[derive(Debug)]
struct DirEdge {
    from: usize,
    to: usize,
    p0: Coord<f64>,
    p1: Coord<f64>,
    quadrant: u8,
    /// true if this edge runs in the same direction as the input line
    forward: bool,
    marked: bool,
    label: i64,
    next: usize,
    ring: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Env {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

impl Env {
    fn of(pts: &[Coord<f64>]) -> Env {
        let mut env =
            Env { min_x: f64::INFINITY, min_y: f64::INFINITY, max_x: f64::NEG_INFINITY, max_y: f64::NEG_INFINITY };
        for c in pts {
            env.min_x = env.min_x.min(c.x);
            env.min_y = env.min_y.min(c.y);
            env.max_x = env.max_x.max(c.x);
            env.max_y = env.max_y.max(c.y);
        }
        env
    }

    fn is_null(&self) -> bool {
        self.min_x > self.max_x
    }

    /// JTS `Envelope.contains` (covers).
    fn contains(&self, o: &Env) -> bool {
        !self.is_null()
            && !o.is_null()
            && o.min_x >= self.min_x
            && o.max_x <= self.max_x
            && o.min_y >= self.min_y
            && o.max_y <= self.max_y
    }

    fn contains_properly(&self, o: &Env) -> bool {
        self != o && self.contains(o)
    }

    fn intersects(&self, o: &Env) -> bool {
        !self.is_null()
            && !o.is_null()
            && !(o.min_x > self.max_x || o.max_x < self.min_x || o.min_y > self.max_y || o.max_y < self.min_y)
    }

    fn area(&self) -> f64 {
        if self.is_null() { 0.0 } else { (self.max_x - self.min_x) * (self.max_y - self.min_y) }
    }

    /// JTS `Envelope.compareTo`.
    fn compare(&self, o: &Env) -> Ordering {
        match (self.is_null(), o.is_null()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }
        let cmp = |a: f64, b: f64| a.partial_cmp(&b).unwrap_or(Ordering::Equal);
        cmp(self.min_x, o.min_x)
            .then_with(|| cmp(self.min_y, o.min_y))
            .then_with(|| cmp(self.max_x, o.max_x))
            .then_with(|| cmp(self.max_y, o.max_y))
    }
}

#[derive(Debug)]
struct EdgeRing {
    des: Vec<usize>,
    /// ring coordinates (closed), consecutive duplicates removed
    pts: Vec<Coord<f64>>,
    env: Env,
    holes: Vec<usize>,
    shell: Option<usize>,
    is_hole: bool,
    is_valid: bool,
    is_processed: bool,
    is_included_set: bool,
    is_included: bool,
}

/// Polygonizes a set of noded line strings (JTS `Polygonizer`).
#[derive(Debug, Default)]
pub struct Polygonizer {
    extract_only_polygonal: bool,
    check_rings_valid: bool,
    nodes: Vec<Node>,
    node_index: HashMap<(u64, u64), usize>,
    lines: Vec<LineString<f64>>,
    dir_edges: Vec<DirEdge>,
    rings: Vec<EdgeRing>,
    computed: bool,
    polygons: Vec<Polygon<f64>>,
    dangles: Vec<LineString<f64>>,
    cut_edges: Vec<LineString<f64>>,
    invalid_ring_lines: Vec<LineString<f64>>,
}

fn coord_key(c: Coord<f64>) -> (u64, u64) {
    // -0.0 == 0.0 like JTS Coordinate.compareTo
    ((c.x + 0.0).to_bits(), (c.y + 0.0).to_bits())
}

/// JTS `Quadrant.quadrant(p0, p1)`: NE = 0, NW = 1, SW = 2, SE = 3.
fn quadrant(p0: Coord<f64>, p1: Coord<f64>) -> u8 {
    match (p1.x >= p0.x, p1.y >= p0.y) {
        (true, true) => 0,
        (true, false) => 3,
        (false, true) => 1,
        (false, false) => 2,
    }
}

fn orientation_index(p: Coord<f64>, q: Coord<f64>, r: Coord<f64>) -> i8 {
    match RobustKernel::orient2d(p, q, r) {
        Orientation::CounterClockwise => 1,
        Orientation::Clockwise => -1,
        Orientation::Collinear => 0,
    }
}

/// JTS `Orientation.isCCW` for a closed ring (`false` for flat or degenerate rings).
pub fn is_ccw(ring: &[Coord<f64>]) -> bool {
    if ring.len() < 4 {
        return false;
    }
    let n_pts = ring.len() - 1;
    let mut up_hi = ring[0];
    let mut prev_y = up_hi.y;
    let mut up_low = up_hi;
    let mut i_up_hi = 0;
    for (i, p) in ring.iter().enumerate().skip(1) {
        if p.y > prev_y && p.y >= up_hi.y {
            up_hi = *p;
            i_up_hi = i;
            up_low = ring[i - 1];
        }
        prev_y = p.y;
    }
    if i_up_hi == 0 {
        return false;
    }
    let mut i_down_low = i_up_hi;
    loop {
        i_down_low = (i_down_low + 1) % n_pts;
        if i_down_low == i_up_hi || ring[i_down_low].y != up_hi.y {
            break;
        }
    }
    let down_low = ring[i_down_low];
    let i_down_hi = if i_down_low > 0 { i_down_low - 1 } else { n_pts - 1 };
    let down_hi = ring[i_down_hi];
    if up_hi == down_hi {
        if up_low == up_hi || down_low == up_hi || up_low == down_low {
            return false;
        }
        orientation_index(up_low, up_hi, down_low) == 1
    } else {
        down_hi.x - up_hi.x < 0.0
    }
}

impl Polygonizer {
    /// A polygonizer that returns every polygon formed (JTS `new Polygonizer()`).
    pub fn new() -> Polygonizer {
        Self::with_extract_only_polygonal(false)
    }

    /// JTS `new Polygonizer(extractOnlyPolygonal)`: if true, only polygons which form a valid polygonal geometry
    /// (no shared edges) are extracted.
    pub fn with_extract_only_polygonal(extract_only_polygonal: bool) -> Polygonizer {
        Polygonizer { extract_only_polygonal, check_rings_valid: true, ..Default::default() }
    }

    /// Whether rings are checked for validity before forming polygons (default true). Disabling this is faster but
    /// may produce invalid polygons if the input is not noded.
    pub fn set_check_rings_valid(&mut self, check: bool) {
        self.check_rings_valid = check;
    }

    /// Adds every line string component of `geom` (including polygon rings, like JTS's component filter).
    pub fn add_geometry(&mut self, geom: &Geometry<f64>) {
        match geom {
            Geometry::LineString(l) => self.add_line(l),
            Geometry::Line(l) => self.add_line(&LineString::from(*l)),
            Geometry::MultiLineString(mls) => mls.0.iter().for_each(|l| self.add_line(l)),
            Geometry::Polygon(p) => self.add_polygon(p),
            Geometry::MultiPolygon(mp) => mp.0.iter().for_each(|p| self.add_polygon(p)),
            Geometry::Rect(r) => self.add_polygon(&r.to_polygon()),
            Geometry::Triangle(t) => self.add_polygon(&t.to_polygon()),
            Geometry::GeometryCollection(gc) => gc.0.iter().for_each(|g| self.add_geometry(g)),
            Geometry::Point(_) | Geometry::MultiPoint(_) => {}
        }
    }

    /// Adds every geometry in `geoms`.
    pub fn add_all<'a>(&mut self, geoms: impl IntoIterator<Item = &'a Geometry<f64>>) {
        for g in geoms {
            self.add_geometry(g);
        }
    }

    fn add_polygon(&mut self, p: &Polygon<f64>) {
        self.add_line(p.exterior());
        p.interiors().iter().for_each(|r| self.add_line(r));
    }

    fn node(&mut self, c: Coord<f64>) -> usize {
        let next = self.nodes.len();
        let idx = *self.node_index.entry(coord_key(c)).or_insert(next);
        if idx == next {
            self.nodes.push(Node { coord: c, out: Vec::new() });
        }
        idx
    }

    /// JTS `PolygonizeGraph.addEdge`.
    pub fn add_line(&mut self, line: &LineString<f64>) {
        assert!(!self.computed, "cannot add lines after polygonizing");
        let mut pts: Vec<Coord<f64>> = Vec::with_capacity(line.0.len());
        for c in &line.0 {
            if pts.last() != Some(c) {
                pts.push(*c);
            }
        }
        if pts.len() < 2 {
            return;
        }
        let start = pts[0];
        let end = pts[pts.len() - 1];
        let n_start = self.node(start);
        let n_end = self.node(end);
        let base = self.dir_edges.len();
        let mk = |from: usize, to: usize, p0: Coord<f64>, p1: Coord<f64>, forward: bool| DirEdge {
            from,
            to,
            p0,
            p1,
            quadrant: quadrant(p0, p1),
            forward,
            marked: false,
            label: -1,
            next: NONE,
            ring: NONE,
        };
        self.dir_edges.push(mk(n_start, n_end, start, pts[1], true));
        self.dir_edges.push(mk(n_end, n_start, end, pts[pts.len() - 2], false));
        self.nodes[n_start].out.push(base);
        self.nodes[n_end].out.push(base + 1);
        self.lines.push(line.clone());
    }

    #[inline]
    fn sym(de: usize) -> usize {
        de ^ 1
    }

    #[inline]
    fn line_of(&self, de: usize) -> &LineString<f64> {
        &self.lines[de / 2]
    }

    /// JTS `DirectedEdge.compareDirection`.
    fn compare_direction(&self, a: usize, b: usize) -> Ordering {
        let (a, b) = (&self.dir_edges[a], &self.dir_edges[b]);
        a.quadrant.cmp(&b.quadrant).then_with(|| orientation_index(b.p0, b.p1, a.p1).cmp(&0))
    }

    fn sort_stars(&mut self) {
        let mut nodes = std::mem::take(&mut self.nodes);
        for node in &mut nodes {
            node.out.sort_by(|&a, &b| self.compare_direction(a, b));
        }
        self.nodes = nodes;
    }

    /// Node indices in JTS `NodeMap` (`TreeMap<Coordinate>`) order.
    fn ordered_nodes(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.nodes.len()).collect();
        order.sort_by(|&a, &b| {
            let (a, b) = (self.nodes[a].coord, self.nodes[b].coord);
            a.x.partial_cmp(&b.x).unwrap_or(Ordering::Equal).then(a.y.partial_cmp(&b.y).unwrap_or(Ordering::Equal))
        });
        order
    }

    fn degree_non_deleted(&self, node: usize) -> usize {
        self.nodes[node].out.iter().filter(|&&de| !self.dir_edges[de].marked).count()
    }

    fn degree_with_label(&self, node: usize, label: i64) -> usize {
        self.nodes[node].out.iter().filter(|&&de| self.dir_edges[de].label == label).count()
    }

    /// JTS `PolygonizeGraph.deleteDangles`.
    fn delete_dangles(&mut self, order: &[usize]) -> Vec<LineString<f64>> {
        let mut stack: Vec<usize> = order.iter().copied().filter(|&n| self.nodes[n].out.len() == 1).collect();
        let mut dangles = Vec::new();
        while let Some(node) = stack.pop() {
            let out = self.nodes[node].out.clone();
            for &de in &out {
                self.dir_edges[de].marked = true;
                self.dir_edges[Self::sym(de)].marked = true;
            }
            for &de in &out {
                dangles.push(self.line_of(de).clone());
                let to = self.dir_edges[de].to;
                if self.degree_non_deleted(to) == 1 {
                    stack.push(to);
                }
            }
        }
        dangles
    }

    /// JTS `PolygonizeGraph.computeNextCWEdges(Node)`.
    fn compute_next_cw_edges(&mut self, node: usize) {
        let mut start = NONE;
        let mut prev = NONE;
        for i in 0..self.nodes[node].out.len() {
            let out = self.nodes[node].out[i];
            if self.dir_edges[out].marked {
                continue;
            }
            if start == NONE {
                start = out;
            }
            if prev != NONE {
                self.dir_edges[Self::sym(prev)].next = out;
            }
            prev = out;
        }
        if prev != NONE {
            self.dir_edges[Self::sym(prev)].next = start;
        }
    }

    fn compute_all_next_cw_edges(&mut self) {
        for node in 0..self.nodes.len() {
            self.compute_next_cw_edges(node);
        }
    }

    /// JTS `PolygonizeGraph.computeNextCCWEdges`.
    fn compute_next_ccw_edges(&mut self, node: usize, label: i64) {
        let mut first_out = NONE;
        let mut prev_in = NONE;
        for i in (0..self.nodes[node].out.len()).rev() {
            let de = self.nodes[node].out[i];
            let sym = Self::sym(de);
            let out_de = if self.dir_edges[de].label == label { de } else { NONE };
            let in_de = if self.dir_edges[sym].label == label { sym } else { NONE };
            if out_de == NONE && in_de == NONE {
                continue;
            }
            if in_de != NONE {
                prev_in = in_de;
            }
            if out_de != NONE {
                if prev_in != NONE {
                    self.dir_edges[prev_in].next = out_de;
                    prev_in = NONE;
                }
                if first_out == NONE {
                    first_out = out_de;
                }
            }
        }
        if prev_in != NONE && first_out != NONE {
            self.dir_edges[prev_in].next = first_out;
        }
    }

    /// The directed edges in the ring starting at `start` (JTS `EdgeRing.findDirEdgesInRing`), following `next`
    /// pointers. Stops early if the pointers are broken (non-noded input).
    fn dir_edges_in_ring(&self, start: usize) -> Vec<usize> {
        let mut edges = Vec::new();
        let mut de = start;
        loop {
            edges.push(de);
            de = self.dir_edges[de].next;
            if de == start || de == NONE || edges.len() > self.dir_edges.len() {
                break;
            }
        }
        edges
    }

    /// JTS `PolygonizeGraph.findLabeledEdgeRings`: labels each edge ring and returns its starting edge.
    fn find_labeled_edge_rings(&mut self) -> Vec<usize> {
        let mut starts = Vec::new();
        let mut label = 1;
        for de in 0..self.dir_edges.len() {
            if self.dir_edges[de].marked || self.dir_edges[de].label >= 0 {
                continue;
            }
            starts.push(de);
            for e in self.dir_edges_in_ring(de) {
                self.dir_edges[e].label = label;
            }
            label += 1;
        }
        starts
    }

    /// JTS `PolygonizeGraph.deleteCutEdges`.
    fn delete_cut_edges(&mut self) -> Vec<LineString<f64>> {
        self.compute_all_next_cw_edges();
        self.find_labeled_edge_rings();
        let mut cut = Vec::new();
        for de in 0..self.dir_edges.len() {
            if self.dir_edges[de].marked {
                continue;
            }
            let sym = Self::sym(de);
            if self.dir_edges[de].label == self.dir_edges[sym].label {
                self.dir_edges[de].marked = true;
                self.dir_edges[sym].marked = true;
                cut.push(self.line_of(de).clone());
            }
        }
        cut
    }

    /// JTS `PolygonizeGraph.findIntersectionNodes`.
    fn find_intersection_nodes(&self, start: usize, label: i64) -> Vec<usize> {
        self.dir_edges_in_ring(start)
            .into_iter()
            .map(|de| self.dir_edges[de].from)
            .filter(|&node| self.degree_with_label(node, label) > 1)
            .collect()
    }

    /// JTS `PolygonizeGraph.getEdgeRings`: builds the minimal edge rings.
    fn build_edge_rings(&mut self) -> Vec<usize> {
        self.compute_all_next_cw_edges();
        for de in &mut self.dir_edges {
            de.label = -1;
        }
        let maximal = self.find_labeled_edge_rings();
        for start in maximal {
            let label = self.dir_edges[start].label;
            for node in self.find_intersection_nodes(start, label) {
                self.compute_next_ccw_edges(node, label);
            }
        }
        let mut result = Vec::new();
        for start in 0..self.dir_edges.len() {
            if self.dir_edges[start].marked || self.dir_edges[start].ring != NONE {
                continue;
            }
            let idx = self.rings.len();
            let mut des = Vec::new();
            let mut de = start;
            loop {
                des.push(de);
                self.dir_edges[de].ring = idx;
                de = self.dir_edges[de].next;
                if de == start || de == NONE || self.dir_edges[de].ring != NONE {
                    break;
                }
            }
            let pts = self.ring_coordinates(&des);
            self.rings.push(EdgeRing {
                des,
                env: Env::of(&pts),
                pts,
                holes: Vec::new(),
                shell: None,
                is_hole: false,
                is_valid: false,
                is_processed: false,
                is_included_set: false,
                is_included: false,
            });
            result.push(idx);
        }
        result
    }

    /// JTS `EdgeRing.getCoordinates`.
    fn ring_coordinates(&self, des: &[usize]) -> Vec<Coord<f64>> {
        let mut pts: Vec<Coord<f64>> = Vec::new();
        let mut add = |c: &Coord<f64>| {
            if pts.last() != Some(c) {
                pts.push(*c);
            }
        };
        for &de in des {
            let line = &self.line_of(de).0;
            if self.dir_edges[de].forward {
                line.iter().for_each(&mut add);
            } else {
                line.iter().rev().for_each(&mut add);
            }
        }
        pts
    }

    fn adjacent_ring(&self, de: usize) -> Option<usize> {
        let r = self.dir_edges[Self::sym(de)].ring;
        (r != NONE).then_some(r)
    }

    /// JTS `EdgeRing.getShell`.
    fn shell_of(&self, ring: usize) -> Option<usize> {
        if self.rings[ring].is_hole { self.rings[ring].shell } else { Some(ring) }
    }

    fn is_outer_hole(&self, ring: usize) -> bool {
        self.rings[ring].is_hole && self.rings[ring].shell.is_none()
    }

    /// JTS `EdgeRing.getOuterHole`.
    fn outer_hole(&self, ring: usize) -> Option<usize> {
        if self.rings[ring].is_hole {
            return None;
        }
        self.rings[ring].des.iter().filter_map(|&de| self.adjacent_ring(de)).find(|&adj| self.is_outer_hole(adj))
    }

    /// JTS `EdgeRing.updateIncluded`.
    fn update_included(&mut self, ring: usize) {
        if self.rings[ring].is_hole {
            return;
        }
        for i in 0..self.rings[ring].des.len() {
            let de = self.rings[ring].des[i];
            if let Some(adj_shell) = self.adjacent_ring(de).and_then(|adj| self.shell_of(adj))
                && self.rings[adj_shell].is_included_set
            {
                let included = !self.rings[adj_shell].is_included;
                self.rings[ring].is_included = included;
                self.rings[ring].is_included_set = true;
                return;
            }
        }
    }

    /// JTS `EdgeRing.contains`: whether `test` is properly inside `ring`.
    fn ring_contains(&self, ring: usize, test: usize) -> bool {
        let (r, t) = (&self.rings[ring], &self.rings[test]);
        if !r.env.contains_properly(&t.env) {
            return false;
        }
        for pt in &t.pts {
            match locate_in_ring(*pt, &r.pts) {
                Location::Interior => return true,
                Location::Exterior => return false,
                Location::Boundary => {}
            }
        }
        false
    }

    /// JTS `HoleAssigner.assignHolesToShells`.
    fn assign_holes_to_shells(&mut self, holes: &[usize], shells: &[usize]) {
        for &hole in holes {
            let env = self.rings[hole].env;
            let mut min: Option<usize> = None;
            for &shell in shells {
                if !self.rings[shell].env.intersects(&env) || !self.ring_contains(shell, hole) {
                    continue;
                }
                if min.is_none_or(|m| self.rings[m].env.contains(&self.rings[shell].env)) {
                    min = Some(shell);
                }
            }
            if let Some(shell) = min {
                self.rings[hole].shell = Some(shell);
                self.rings[shell].holes.push(hole);
            }
        }
    }

    /// JTS `Polygonizer.isIncludedInvalid`.
    fn is_included_invalid(&self, ring: usize) -> bool {
        self.rings[ring].des.iter().any(|&de| match self.adjacent_ring(de) {
            Some(adj) => !(self.rings[adj].is_valid || self.rings[adj].is_processed),
            None => true,
        })
    }

    fn polygonize(&mut self) {
        if self.computed {
            return;
        }
        self.computed = true;
        if self.dir_edges.is_empty() {
            return;
        }
        self.sort_stars();
        let order = self.ordered_nodes();
        self.dangles = self.delete_dangles(&order);
        self.cut_edges = self.delete_cut_edges();
        let edge_rings = self.build_edge_rings();

        let mut valid = Vec::new();
        if self.check_rings_valid {
            let mut invalid = Vec::new();
            for &r in &edge_rings {
                let ring = &mut self.rings[r];
                ring.is_valid =
                    ring.pts.len() > 3 && is_valid_polygons(&[Polygon::new(LineString(ring.pts.clone()), vec![])]);
                if ring.is_valid { valid.push(r) } else { invalid.push(r) }
            }
            invalid.sort_by(|&a, &b| self.rings[a].env.area().total_cmp(&self.rings[b].env.area()));
            for r in invalid {
                if self.is_included_invalid(r) {
                    self.invalid_ring_lines.push(LineString(self.rings[r].pts.clone()));
                }
                self.rings[r].is_processed = true;
            }
        } else {
            // JTS can't build a LinearRing from fewer than 4 points
            valid = edge_rings.into_iter().filter(|&r| self.rings[r].pts.len() >= 4).collect();
        }

        let mut holes = Vec::new();
        let mut shells = Vec::new();
        for r in valid {
            let hole = is_ccw(&self.rings[r].pts);
            self.rings[r].is_hole = hole;
            if hole { holes.push(r) } else { shells.push(r) }
        }
        self.assign_holes_to_shells(&holes, &shells);
        shells.sort_by(|&a, &b| self.rings[a].env.compare(&self.rings[b].env));

        let mut include_all = true;
        if self.extract_only_polygonal {
            self.find_disjoint_shells(&shells);
            include_all = false;
        }
        self.polygons = shells
            .iter()
            .filter(|&&r| include_all || self.rings[r].is_included)
            .map(|&r| {
                let ring = &self.rings[r];
                let holes = ring.holes.iter().map(|&h| LineString(self.rings[h].pts.clone())).collect();
                Polygon::new(LineString(ring.pts.clone()), holes)
            })
            .collect();
    }

    /// JTS `Polygonizer.findDisjointShells`.
    fn find_disjoint_shells(&mut self, shells: &[usize]) {
        for &r in shells {
            if let Some(outer_hole) = self.outer_hole(r)
                && !self.rings[outer_hole].is_processed
            {
                self.rings[r].is_included = true;
                self.rings[r].is_included_set = true;
                self.rings[outer_hole].is_processed = true;
            }
        }
        loop {
            let mut more = false;
            let mut progress = false;
            for &r in shells {
                if self.rings[r].is_included_set {
                    continue;
                }
                self.update_included(r);
                if self.rings[r].is_included_set {
                    progress = true;
                } else {
                    more = true;
                }
            }
            // JTS would loop forever here if no remaining shell can be decided
            if !more || !progress {
                break;
            }
        }
    }

    /// The polygons formed (JTS `getPolygons`), shells ordered by envelope.
    pub fn polygons(&mut self) -> &[Polygon<f64>] {
        self.polygonize();
        &self.polygons
    }

    /// JTS `getGeometry`: a polygonal geometry if `extract_only_polygonal`, otherwise a geometry collection of the
    /// polygons (which may overlap or share edges).
    pub fn geometry(&mut self) -> Geometry<f64> {
        self.polygonize();
        let polys = self.polygons.clone();
        if self.extract_only_polygonal {
            match polys.len() {
                0 => Geometry::GeometryCollection(GeometryCollection(vec![])),
                1 => Geometry::Polygon(polys.into_iter().next().unwrap()),
                _ => Geometry::MultiPolygon(MultiPolygon(polys)),
            }
        } else {
            Geometry::GeometryCollection(GeometryCollection(polys.into_iter().map(Geometry::Polygon).collect()))
        }
    }

    /// Lines that end at a node not shared with another line (JTS `getDangles`).
    pub fn dangles(&mut self) -> &[LineString<f64>] {
        self.polygonize();
        &self.dangles
    }

    /// Lines with the same face on both sides (JTS `getCutEdges`).
    pub fn cut_edges(&mut self) -> &[LineString<f64>] {
        self.polygonize();
        &self.cut_edges
    }

    /// Rings that do not form valid polygons (JTS `getInvalidRingLines`).
    pub fn invalid_ring_lines(&mut self) -> &[LineString<f64>] {
        self.polygonize();
        &self.invalid_ring_lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::normalize::{compare_geometries, norm};
    use crate::geo::wkt::read_wkt;

    fn read_all(wkts: &[&str]) -> Vec<Geometry<f64>> {
        wkts.iter().map(|w| read_wkt(w).unwrap()).collect()
    }

    fn read_line(wkt: &str) -> LineString<f64> {
        match read_wkt(wkt).unwrap() {
            Geometry::LineString(l) => l,
            other => panic!("{other:?}"),
        }
    }

    fn sorted_norm(geoms: Vec<Geometry<f64>>) -> Vec<Geometry<f64>> {
        let mut out: Vec<Geometry<f64>> = geoms.iter().map(norm).collect();
        out.sort_by(compare_geometries);
        out
    }

    fn check_polygonize_with(extract_only_polygonal: bool, input: &[&str], expected: &[&str]) {
        let mut polygonizer = Polygonizer::with_extract_only_polygonal(extract_only_polygonal);
        polygonizer.add_all(&read_all(input));
        let actual: Vec<Geometry<f64>> = polygonizer.polygons().iter().cloned().map(Geometry::Polygon).collect();
        assert_eq!(sorted_norm(read_all(expected)), sorted_norm(actual));
    }

    fn check_polygonize(input: &[&str], expected: &[&str]) {
        check_polygonize_with(false, input, expected);
    }

    /// Linework as a set of undirected segments (JTS `equalsTopo` on these inputs).
    fn segments(lines: &[LineString<f64>]) -> Vec<[(u64, u64); 2]> {
        let mut out: Vec<[(u64, u64); 2]> = lines
            .iter()
            .flat_map(|l| l.0.windows(2).map(|w| [coord_key(w[0]), coord_key(w[1])]))
            .map(|mut s| {
                s.sort();
                s
            })
            .collect();
        out.sort();
        out.dedup();
        out
    }

    fn check_invalid_rings(input: &str, expected: &str) {
        let mut polygonizer = Polygonizer::new();
        polygonizer.add_geometry(&read_wkt(input).unwrap());
        let actual = polygonizer.invalid_ring_lines().to_vec();
        let expected: Vec<LineString<f64>> = match read_wkt(expected).unwrap() {
            Geometry::LineString(l) => vec![l],
            Geometry::MultiLineString(m) => m.0,
            other => panic!("{other:?}"),
        };
        assert_eq!(expected.len(), actual.len());
        assert_eq!(segments(&expected), segments(&actual));
    }

    #[test]
    fn test_empty_input() {
        check_polygonize(&["LINESTRING EMPTY", "LINESTRING EMPTY"], &[]);
    }

    #[test]
    fn test_polygon_with_touching_hole() {
        check_polygonize(
            &["LINESTRING (100 180, 20 20, 160 20, 100 180)", "LINESTRING (100 180, 80 60, 120 60, 100 180)"],
            &[
                "POLYGON ((100 180, 120 60, 80 60, 100 180))",
                "POLYGON ((100 180, 160 20, 20 20, 100 180), (100 180, 80 60, 120 60, 100 180))",
            ],
        );
    }

    #[test]
    fn test_polygon_with_touching_hole_and_notch() {
        check_polygonize(
            &[
                "LINESTRING (0 0, 4 0)",
                "LINESTRING (4 0, 5 3)",
                "LINESTRING (5 3, 4 6, 6 6, 5 3)",
                "LINESTRING (5 3, 6 0)",
                "LINESTRING (6 0, 10 0, 5 10, 0 0)",
                "LINESTRING (4 0, 6 0)",
            ],
            &[
                "POLYGON ((5 3, 4 0, 0 0, 5 10, 10 0, 6 0, 5 3), (5 3, 6 6, 4 6, 5 3))",
                "POLYGON ((5 3, 4 6, 6 6, 5 3))",
                "POLYGON ((4 0, 5 3, 6 0, 4 0))",
            ],
        );
    }

    #[test]
    fn test_polygonal1() {
        check_polygonize_with(
            true,
            &[
                "LINESTRING (100 100, 100 300, 300 300, 300 100, 100 100)",
                "LINESTRING (150 150, 150 250, 250 250, 250 150, 150 150)",
            ],
            &["POLYGON ((100 100, 100 300, 300 300, 300 100, 100 100), (150 150, 150 250, 250 250, 250 150, 150 150))"],
        );
    }

    #[test]
    fn test_polygonal2() {
        check_polygonize_with(
            true,
            &[
                "LINESTRING (100 100, 100 0, 0 0, 0 100, 100 100)",
                "LINESTRING (10 10, 10 30, 20 30)",
                "LINESTRING (20 30, 30 30, 30 20)",
                "LINESTRING (30 20, 30 10, 10 10)",
                "LINESTRING (40 40, 40 20, 30 20)",
                "LINESTRING (30 20, 20 20, 20 30)",
                "LINESTRING (20 30, 20 40, 40 40)",
            ],
            &[
                "POLYGON ((0 0, 0 100, 100 100, 100 0, 0 0), (10 10, 30 10, 30 20, 40 20, 40 40, 20 40, 20 30, 10 30, 10 10))",
                "POLYGON ((20 20, 20 30, 30 30, 30 20, 20 20))",
            ],
        );
    }

    #[test]
    fn test_polygonal_outer_only_1() {
        check_polygonize_with(
            true,
            &[
                "LINESTRING (10 10, 10 20, 20 20)",
                "LINESTRING (20 20, 20 10)",
                "LINESTRING (20 10, 10 10)",
                "LINESTRING (20 20, 30 20, 30 10, 20 10)",
            ],
            &["POLYGON ((20 20, 20 10, 10 10, 10 20, 20 20))"],
        );
    }

    #[test]
    fn test_polygonal_outer_only_2() {
        check_polygonize_with(
            true,
            &[
                "LINESTRING (100 400, 200 400, 200 300)",
                "LINESTRING (200 300, 150 300)",
                "LINESTRING (150 300, 100 300, 100 400)",
                "LINESTRING (200 300, 250 300, 250 200)",
                "LINESTRING (250 200, 200 200)",
                "LINESTRING (200 200, 150 200, 150 300)",
                "LINESTRING (250 200, 300 200, 300 100, 200 100, 200 200)",
            ],
            &[
                "POLYGON ((150 300, 100 300, 100 400, 200 400, 200 300, 150 300))",
                "POLYGON ((200 200, 250 200, 300 200, 300 100, 200 100, 200 200))",
            ],
        );
    }

    const LINES_CHECKERBOARD: &[&str] = &[
        "LINESTRING (10 20, 20 20)",
        "LINESTRING (10 20, 10 30)",
        "LINESTRING (20 10, 10 10, 10 20)",
        "LINESTRING (10 30, 20 30)",
        "LINESTRING (10 30, 10 40, 20 40)",
        "LINESTRING (30 10, 20 10)",
        "LINESTRING (20 20, 20 10)",
        "LINESTRING (20 20, 30 20)",
        "LINESTRING (20 30, 20 20)",
        "LINESTRING (20 30, 30 30)",
        "LINESTRING (20 40, 20 30)",
        "LINESTRING (20 40, 30 40)",
        "LINESTRING (40 20, 40 10, 30 10)",
        "LINESTRING (30 20, 30 10)",
        "LINESTRING (30 20, 40 20)",
        "LINESTRING (30 30, 30 20)",
        "LINESTRING (30 30, 40 30)",
        "LINESTRING (30 40, 30 30)",
        "LINESTRING (30 40, 40 40, 40 30)",
        "LINESTRING (40 30, 40 20)",
    ];

    #[test]
    fn test_polygonal_outer_only_checkerboard() {
        check_polygonize_with(
            true,
            LINES_CHECKERBOARD,
            &[
                "POLYGON ((10 20, 20 20, 20 10, 10 10, 10 20))",
                "POLYGON ((20 30, 10 30, 10 40, 20 40, 20 30))",
                "POLYGON ((30 20, 20 20, 20 30, 30 30, 30 20))",
                "POLYGON ((30 10, 30 20, 40 20, 40 10, 30 10))",
                "POLYGON ((30 40, 40 40, 40 30, 30 30, 30 40))",
            ],
        );
    }

    #[test]
    fn test_checkerboard_all_faces() {
        let mut polygonizer = Polygonizer::new();
        polygonizer.add_all(&read_all(LINES_CHECKERBOARD));
        assert_eq!(9, polygonizer.polygons().len());
        assert!(polygonizer.dangles().is_empty());
        assert!(polygonizer.cut_edges().is_empty());
    }

    /// Input is invalid (non-noded), but Polygonizer should still not fail. Output is undefined, however.
    #[test]
    fn test_non_noded_with_hole_not_assignable() {
        let mut polygonizer = Polygonizer::new();
        polygonizer.add_geometry(
            &read_wkt(
                "MULTILINESTRING ((10 90, 30 90, 30 30, 70 30, 70 90, 90 90, 90 10, 10 10, 10 90), (30 90, 70 90, 70 30, 30 30, 30 90))",
            )
            .unwrap(),
        );
        polygonizer.polygons();
    }

    #[test]
    fn test_unique_invalid_rings() {
        check_invalid_rings(
            "MULTILINESTRING ((0 0, 2 0, 2 2, 0 2, 0 0), (0 0, 1 1), (2 2, 4 4, 2 4, 4 2, 2 2))",
            "LINESTRING (2 2, 4 2, 2 4, 4 4, 2 2)",
        );
    }

    #[test]
    fn test_unique_invalid_rings2() {
        check_invalid_rings(
            "MULTILINESTRING ((3 8, 7 8), (7 8, 7 3), (7 3, 3 3), (3 3, 3 8), (7 3, 9 6, 9 5, 7 8), (3 8, 1 5), (1 5, 1 6), (1 6, 3 3))",
            "MULTILINESTRING ((3 8, 3 3, 1 6, 1 5, 3 8), (7 3, 7 8, 9 5, 9 6, 7 3))",
        );
    }

    #[test]
    fn test_dangles_and_cut_edges() {
        let mut polygonizer = Polygonizer::new();
        polygonizer.add_all(&read_all(&[
            // square, noded at (10 0) and (10 10)
            "LINESTRING (10 0, 10 10)",
            "LINESTRING (10 10, 0 10, 0 0, 10 0)",
            // dangle off a corner
            "LINESTRING (10 10, 15 15)",
            // cut edge to a second square
            "LINESTRING (10 0, 20 0)",
            "LINESTRING (20 0, 30 0, 30 10, 20 10, 20 0)",
        ]));
        assert_eq!(2, polygonizer.polygons().len());
        assert_eq!(&[read_line("LINESTRING (10 10, 15 15)")], polygonizer.dangles());
        assert_eq!(&[read_line("LINESTRING (10 0, 20 0)")], polygonizer.cut_edges());
        assert!(polygonizer.invalid_ring_lines().is_empty());
    }

    #[test]
    fn test_unclosed_lines_are_dangles() {
        let mut polygonizer = Polygonizer::new();
        polygonizer.add_all(&read_all(&["LINESTRING (0 0, 10 0, 10 10)", "LINESTRING (10 10, 0 10)"]));
        assert!(polygonizer.polygons().is_empty());
        assert!(!polygonizer.dangles().is_empty());
        assert!(matches!(polygonizer.geometry(), Geometry::GeometryCollection(gc) if gc.0.is_empty()));
    }

    #[test]
    fn test_is_ccw() {
        let ring = |w: &str| read_line(w).0;
        assert!(is_ccw(&ring("LINESTRING (0 0, 10 0, 10 10, 0 10, 0 0)")));
        assert!(!is_ccw(&ring("LINESTRING (0 0, 0 10, 10 10, 10 0, 0 0)")));
        assert!(!is_ccw(&ring("LINESTRING (0 0, 10 0, 0 0)")));
    }
}

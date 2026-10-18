//! Port of `util/LoopLineMerger.java`.
//!
//! A utility for merging, simplifying, and connecting linestrings and removing small loops.
//!
//! Compared to JTS `LineMerger` (see [`line_merger`]) which only connects when 2 lines meet at a single point, this
//! utility:
//! - snap-rounds points to a grid
//! - splits lines that intersect at a midpoint
//! - breaks small loops less than `loop_min_length` so only the shortest path connects both endpoints of the loop
//! - removes short "hair" edges less than `stub_min_length` coming off the side of longer segments
//! - simplifies linestrings, without touching the points shared between multiple lines to avoid breaking connections
//! - removes any duplicate edges
//! - at any remaining 3+ way intersections, connect pairs of edges that form the straightest path through the node
//! - remove any remaining edges shorter than `min_length`
//!
//! See [Improving Linestring Merging in
//! Planetiler](https://oliverwipfli.ch/improving-linestring-merging-in-planetiler-2024-10-30/).

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use geo::CoordsIter;
use geo_types::{Coord, Geometry, LineString};
use hashbrown::HashMap;

use crate::geo::douglas_peucker_simplifier;
use crate::geo::geometry_pipeline::GeometryPipelineRef;
use crate::geo::precision_model::PrecisionModel;

type NodeId = u32;
/// Directed edge id. Edges are created in pairs: the even id is the "main" edge in the direction of the input line,
/// `id ^ 1` is the same edge reversed. Both share one coordinate range (stored in the main edge's direction).
type EdgeId = u32;

const NO_NODE: u32 = u32::MAX;

/// List of edges attached to a node; stored inline for the common case of up to 4 edges.
enum EdgeList {
    Inline(u8, [EdgeId; 4]),
    Heap(Vec<EdgeId>),
}

impl EdgeList {
    #[inline]
    fn new() -> Self {
        EdgeList::Inline(0, [0; 4])
    }

    #[inline]
    fn as_slice(&self) -> &[EdgeId] {
        match self {
            EdgeList::Inline(len, items) => &items[..*len as usize],
            EdgeList::Heap(v) => v,
        }
    }

    #[inline]
    fn len(&self) -> usize {
        match self {
            EdgeList::Inline(len, _) => *len as usize,
            EdgeList::Heap(v) => v.len(),
        }
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    fn get(&self, i: usize) -> EdgeId {
        self.as_slice()[i]
    }

    fn push(&mut self, edge: EdgeId) {
        match self {
            EdgeList::Inline(len, items) if (*len as usize) < items.len() => {
                items[*len as usize] = edge;
                *len += 1;
            }
            EdgeList::Inline(len, items) => {
                let mut v = Vec::with_capacity(8);
                v.extend_from_slice(&items[..*len as usize]);
                v.push(edge);
                *self = EdgeList::Heap(v);
            }
            EdgeList::Heap(v) => v.push(edge),
        }
    }

    /// Removes the first occurrence of `edge`, preserving the order of the others.
    fn remove_item(&mut self, edge: EdgeId) {
        match self {
            EdgeList::Inline(len, items) => {
                let n = *len as usize;
                if let Some(pos) = items[..n].iter().position(|e| *e == edge) {
                    items.copy_within(pos + 1..n, pos);
                    *len -= 1;
                }
            }
            EdgeList::Heap(v) => {
                if let Some(pos) = v.iter().position(|e| *e == edge) {
                    v.remove(pos);
                }
            }
        }
    }
}

struct Node {
    coordinate: Coord<f64>,
    edges: EdgeList,
}

/// One direction of an edge.
#[derive(Clone, Copy)]
struct Edge {
    from: NodeId,
    to: NodeId,
    removed: bool,
}

/// Data shared by both directions of an edge.
#[derive(Clone, Copy)]
struct EdgePair {
    length: f64,
    /// range of the coordinates (in the direction of the main edge) in [`LoopLineMerger::coords`]
    start: u32,
    len: u32,
}

#[inline]
fn is_main(edge: EdgeId) -> bool {
    edge & 1 == 0
}

#[inline]
fn reversed(edge: EdgeId) -> EdgeId {
    edge ^ 1
}

/// Hash key for a coordinate with `-0.0` normalized to `0.0` so it matches Java `Coordinate.equals`.
#[inline]
fn coord_key(c: &Coord<f64>) -> (u64, u64) {
    let norm = |v: f64| if v == 0.0 { 0u64 } else { v.to_bits() };
    (norm(c.x), norm(c.y))
}

#[inline]
fn distance(a: &Coord<f64>, b: &Coord<f64>) -> f64 {
    (a.x - b.x).hypot(a.y - b.y)
}

/// JTS `Angle.angle(p0, p1)`.
#[inline]
fn angle(p0: &Coord<f64>, p1: &Coord<f64>) -> f64 {
    (p1.y - p0.y).atan2(p1.x - p0.x)
}

/// JTS `Angle.normalize`: normalize to `(-PI, PI]`.
#[inline]
fn normalize_angle(mut angle: f64) -> f64 {
    use std::f64::consts::PI;
    while angle > PI {
        angle -= 2.0 * PI;
    }
    while angle <= -PI {
        angle += 2.0 * PI;
    }
    angle
}

/// Reusable buffers for the graph algorithms (kept between calls to avoid allocating in inner loops).
#[derive(Default)]
struct Scratch {
    edges: Vec<EdgeId>,
    angled_pairs: Vec<(EdgeId, EdgeId, f64)>,
    merged: Vec<EdgeId>,
    loops: Vec<(EdgeId, f64)>,
    to_remove: Vec<EdgeId>,
    coords: Vec<Coord<f64>>,
    /// A* best distance per node, valid when `best_stamp[node] == stamp`
    best_distance: Vec<f64>,
    best_stamp: Vec<u32>,
    stamp: u32,
    frontier: BinaryHeap<Candidate>,
}

/// Merges, simplifies and connects linestrings (see module docs).
pub struct LoopLineMerger {
    /// coordinates of all input lines, back to back
    input_coords: Vec<Coord<f64>>,
    /// end offset of each input line in `input_coords`
    input_ends: Vec<usize>,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    pairs: Vec<EdgePair>,
    /// arena holding the coordinates of every edge; merged and simplified edges append new ranges
    coords: Vec<Coord<f64>>,
    output: Vec<NodeId>,
    scratch: Scratch,
    precision_model: PrecisionModel,
    min_length: f64,
    loop_min_length: f64,
    stub_min_length: f64,
    tolerance: f64,
    merge_strokes: bool,
    pipeline: Option<GeometryPipelineRef>,
}

impl Default for LoopLineMerger {
    fn default() -> Self {
        Self::new()
    }
}

/// Port of Java's `PriorityQueue` binary heap so that ties are broken in exactly the same order.
struct JavaPriorityQueue<T, F: Fn(&T, &T) -> Ordering> {
    queue: Vec<T>,
    cmp: F,
}

impl<T: Copy, F: Fn(&T, &T) -> Ordering> JavaPriorityQueue<T, F> {
    fn new(cmp: F) -> Self {
        JavaPriorityQueue { queue: Vec::new(), cmp }
    }

    fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    fn offer(&mut self, x: T) {
        let mut k = self.queue.len();
        self.queue.push(x);
        while k > 0 {
            let parent = (k - 1) >> 1;
            let e = self.queue[parent];
            if (self.cmp)(&x, &e) != Ordering::Less {
                break;
            }
            self.queue[k] = e;
            k = parent;
        }
        self.queue[k] = x;
    }

    fn poll(&mut self) -> Option<T> {
        if self.queue.is_empty() {
            return None;
        }
        let result = self.queue[0];
        let x = self.queue.pop().unwrap();
        let n = self.queue.len();
        if n > 0 {
            let mut k = 0;
            let half = n >> 1;
            while k < half {
                let mut child = 2 * k + 1;
                let mut c = self.queue[child];
                let right = child + 1;
                if right < n && (self.cmp)(&c, &self.queue[right]) == Ordering::Greater {
                    child = right;
                    c = self.queue[child];
                }
                if (self.cmp)(&x, &c) != Ordering::Greater {
                    break;
                }
                self.queue[k] = c;
                k = child;
            }
            self.queue[k] = x;
        }
        Some(result)
    }
}

#[derive(Clone, Copy)]
struct Candidate {
    node: NodeId,
    length: f64,
    min_total_length: f64,
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Candidate {}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        // reversed so BinaryHeap is a min-heap
        other.min_total_length.total_cmp(&self.min_total_length)
    }
}

impl LoopLineMerger {
    pub fn new() -> Self {
        LoopLineMerger {
            input_coords: Vec::new(),
            input_ends: Vec::new(),
            nodes: Vec::new(),
            edges: Vec::new(),
            pairs: Vec::new(),
            coords: Vec::new(),
            output: Vec::new(),
            scratch: Scratch::default(),
            precision_model: PrecisionModel::fixed(crate::geo::geo_utils::TILE_PRECISION_SCALE),
            min_length: 0.0,
            loop_min_length: 0.0,
            stub_min_length: 0.0,
            tolerance: -1.0,
            merge_strokes: false,
            pipeline: None,
        }
    }

    /// Sets the precision model used to snap points to a grid ([`PrecisionModel::Floating`] to not snap at all).
    pub fn set_precision_model(&mut self, precision_model: PrecisionModel) -> &mut Self {
        self.precision_model = precision_model;
        self
    }

    /// Sets the minimum length for retaining linestrings in the resulting geometry (`<= 0` disables).
    pub fn set_min_length(&mut self, min_length: f64) -> &mut Self {
        self.min_length = min_length;
        self
    }

    /// Sets the minimum loop length for breaking loops in the merged geometry (`<= 0` disables).
    pub fn set_loop_min_length(&mut self, loop_min_length: f64) -> &mut Self {
        self.loop_min_length = loop_min_length;
        self
    }

    /// Sets the minimum length of stubs to be removed during processing (`<= 0` disables).
    pub fn set_stub_min_length(&mut self, stub_min_length: f64) -> &mut Self {
        self.stub_min_length = stub_min_length;
        self
    }

    /// Sets the tolerance for simplifying linestrings during processing (`< 0` disables, `0` removes collinear
    /// points).
    pub fn set_tolerance(&mut self, tolerance: f64) -> &mut Self {
        self.tolerance = tolerance;
        self
    }

    /// Sets a function that should be applied to each segment between intersections instead of simplification.
    pub fn set_segment_transform(&mut self, pipeline: GeometryPipelineRef) -> &mut Self {
        self.pipeline = Some(pipeline);
        self
    }

    /// Enables or disables stroke merging at junctions with 3 or more attached linestrings.
    pub fn set_merge_strokes(&mut self, merge_strokes: bool) -> &mut Self {
        self.merge_strokes = merge_strokes;
        self
    }

    fn add_coords(&mut self, coords: &[Coord<f64>]) {
        self.input_coords.extend_from_slice(coords);
        self.input_ends.push(self.input_coords.len());
    }

    /// Adds a geometry to the merger. Only linestrings (including polygon rings) from the input are considered.
    pub fn add(&mut self, geometry: &Geometry<f64>) -> &mut Self {
        match geometry {
            Geometry::LineString(ls) => self.add_coords(&ls.0),
            Geometry::Line(l) => self.add_coords(&[l.start, l.end]),
            Geometry::MultiLineString(mls) => {
                let total: usize = mls.0.iter().map(|ls| ls.0.len()).sum();
                self.input_coords.reserve(total);
                self.input_ends.reserve(mls.0.len());
                for ls in &mls.0 {
                    self.add_coords(&ls.0);
                }
            }
            Geometry::Polygon(p) => {
                self.add_coords(&p.exterior().0);
                for ring in p.interiors() {
                    self.add_coords(&ring.0);
                }
            }
            Geometry::MultiPolygon(mp) => {
                for p in &mp.0 {
                    self.add_coords(&p.exterior().0);
                    for ring in p.interiors() {
                        self.add_coords(&ring.0);
                    }
                }
            }
            Geometry::Rect(r) => {
                self.add(&Geometry::Polygon(r.to_polygon()));
            }
            Geometry::Triangle(t) => {
                self.add(&Geometry::Polygon(t.to_polygon()));
            }
            Geometry::GeometryCollection(gc) => {
                for g in &gc.0 {
                    self.add(g);
                }
            }
            Geometry::Point(_) | Geometry::MultiPoint(_) => {}
        }
        self
    }

    // ---- graph primitives ----

    #[inline]
    fn edge(&self, edge: EdgeId) -> &Edge {
        &self.edges[edge as usize]
    }

    #[inline]
    fn pair(&self, edge: EdgeId) -> &EdgePair {
        &self.pairs[(edge >> 1) as usize]
    }

    #[inline]
    fn node(&self, node: NodeId) -> &Node {
        &self.nodes[node as usize]
    }

    #[inline]
    fn node_edges(&self, node: NodeId) -> &[EdgeId] {
        self.nodes[node as usize].edges.as_slice()
    }

    #[inline]
    fn length(&self, edge: EdgeId) -> f64 {
        self.pair(edge).length
    }

    /// Coordinates of `edge` in the direction of its main edge.
    #[inline]
    fn pair_coords(&self, edge: EdgeId) -> &[Coord<f64>] {
        let p = self.pair(edge);
        &self.coords[p.start as usize..(p.start + p.len) as usize]
    }

    /// The `i`th coordinate of `edge` in its direction.
    #[inline]
    fn coord_at(&self, edge: EdgeId, i: usize) -> Coord<f64> {
        let coords = self.pair_coords(edge);
        if is_main(edge) { coords[i] } else { coords[coords.len() - 1 - i] }
    }

    /// True if `a` and `b` have the same coordinates (in their respective directions).
    fn same_coordinates(&self, a: EdgeId, b: EdgeId) -> bool {
        let ca = self.pair_coords(a);
        let cb = self.pair_coords(b);
        if ca.len() != cb.len() {
            false
        } else if is_main(a) == is_main(b) {
            ca == cb
        } else {
            ca.iter().eq(cb.iter().rev())
        }
    }

    fn new_edge(&mut self, from: NodeId, to: NodeId, start: usize, len: usize, length: f64) -> EdgeId {
        let id = self.edges.len() as EdgeId;
        debug_assert!(is_main(id));
        self.edges.push(Edge { from, to, removed: false });
        self.edges.push(Edge { from: to, to: from, removed: false });
        self.pairs.push(EdgePair { length, start: start as u32, len: len as u32 });
        id
    }

    fn add_edge(&mut self, node: NodeId, edge: EdgeId) {
        for &other in self.node_edges(node) {
            if self.same_coordinates(other, edge) {
                return;
            }
        }
        self.nodes[node as usize].edges.push(edge);
    }

    #[inline]
    fn remove_edge_from_node(&mut self, node: NodeId, edge: EdgeId) {
        self.nodes[node as usize].edges.remove_item(edge);
    }

    fn remove(&mut self, edge: EdgeId) {
        let e = *self.edge(edge);
        if !e.removed {
            self.remove_edge_from_node(e.from, edge);
            self.remove_edge_from_node(e.to, reversed(edge));
            self.edges[edge as usize].removed = true;
        }
    }

    #[inline]
    fn is_loop(&self, edge: EdgeId) -> bool {
        let e = self.edge(edge);
        e.from == e.to
    }

    fn angle_to(&self, a: EdgeId, b: EdgeId) -> f64 {
        let angle_a = angle(&self.coord_at(a, 0), &self.coord_at(a, 1));
        let angle_b = angle(&self.coord_at(b, 0), &self.coord_at(b, 1));
        normalize_angle(angle_a - angle_b).abs()
    }

    // ---- algorithm steps ----

    fn degree_two_merge_all(&mut self) {
        for i in 0..self.output.len() {
            let node = self.output[i];
            self.degree_two_merge(node);
        }
        let nodes = &self.nodes;
        self.output.retain(|n| !nodes[*n as usize].edges.is_empty());
    }

    fn degree_two_merge(&mut self, node: NodeId) -> Option<EdgeId> {
        let edges = &self.node(node).edges;
        if edges.len() == 2 {
            let a = edges.get(0);
            let b = edges.get(1);
            // if one side is a loop, degree is actually > 2
            if !self.is_loop(a) && !self.is_loop(b) {
                return Some(self.merge_two_edges(node, a, b));
            }
        }
        None
    }

    /// Appends the coordinates of `edge` (in its direction, skipping the first `skip`) to the coordinate arena.
    fn append_coords(&mut self, edge: EdgeId, skip: usize) {
        let p = *self.pair(edge);
        let (start, end) = (p.start as usize, (p.start + p.len) as usize);
        if is_main(edge) {
            self.coords.extend_from_within(start + skip.min(p.len as usize)..end);
        } else {
            let n = (p.len as usize).saturating_sub(skip);
            self.coords.reserve(n);
            for i in (start..start + n).rev() {
                let c = self.coords[i];
                self.coords.push(c);
            }
        }
    }

    fn merge_two_edges(&mut self, node: NodeId, edge1: EdgeId, edge2: EdgeId) -> EdgeId {
        // attempt to preserve segment directions from the original line
        // when: A << N -- B then output C reversed from B to A
        // when: A >> N -- B then output C from A to B
        let (a, b) = if is_main(edge1) { (edge2, edge1) } else { (edge1, edge2) };
        self.remove_edge_from_node(node, a);
        self.remove_edge_from_node(node, b);
        let (ea, eb) = (*self.edge(a), *self.edge(b));
        let length = self.length(a) + self.length(b);
        // coordinates of a in reverse, then b without its first point
        let start = self.coords.len();
        self.append_coords(reversed(a), 0);
        self.append_coords(b, 1);
        let c = self.new_edge(ea.to, eb.to, start, self.coords.len() - start, length);
        self.remove_edge_from_node(ea.to, reversed(a));
        self.remove_edge_from_node(eb.to, reversed(b));
        self.add_edge(ea.to, c);
        if ea.to != eb.to {
            self.add_edge(eb.to, reversed(c));
        }
        c
    }

    fn stroke_merge(&mut self) {
        let mut edges = std::mem::take(&mut self.scratch.edges);
        let mut angled_pairs = std::mem::take(&mut self.scratch.angled_pairs);
        let mut merged = std::mem::take(&mut self.scratch.merged);
        for i in 0..self.output.len() {
            let node = self.output[i];
            if self.node(node).edges.len() >= 2 {
                edges.clear();
                edges.extend_from_slice(self.node_edges(node));
                angled_pairs.clear();
                for (ii, &edge_i) in edges.iter().enumerate() {
                    for &edge_j in &edges[ii + 1..] {
                        if edge_i != reversed(edge_j) {
                            angled_pairs.push((edge_i, edge_j, self.angle_to(edge_i, edge_j)));
                        }
                    }
                }
                angled_pairs.sort_by(|a, b| a.2.total_cmp(&b.2));
                merged.clear();
                for &(a, b, _) in angled_pairs.iter().rev() {
                    if merged.contains(&a) || merged.contains(&b) {
                        continue;
                    }
                    let from = self.edge(a).from;
                    self.merge_two_edges(from, a, b);
                    merged.push(a);
                    merged.push(b);
                }
            }
        }
        self.scratch.edges = edges;
        self.scratch.angled_pairs = angled_pairs;
        self.scratch.merged = merged;
    }

    fn break_loops(&mut self) {
        let mut current_edges = std::mem::take(&mut self.scratch.edges);
        let mut loops = std::mem::take(&mut self.scratch.loops);
        for i in 0..self.output.len() {
            let node = self.output[i];
            if self.node(node).edges.len() <= 1 {
                continue;
            }
            current_edges.clear();
            current_edges.extend_from_slice(self.node_edges(node));
            for &current in &current_edges {
                if !self.node_edges(node).contains(&current) {
                    continue;
                }
                loops.clear();
                let Edge { from: cur_from, to: cur_to, .. } = *self.edge(current);
                for j in 0..self.node(node).edges.len() {
                    let other = self.node(node).edges.get(j);
                    let other_length = self.length(other);
                    let other_to = self.edge(other).to;
                    let distance = other_length
                        + self.shortest_distance_a_star(
                            other_to,
                            cur_to,
                            cur_from,
                            self.loop_min_length - other_length,
                        );
                    if distance <= self.loop_min_length {
                        loops.push((other, distance));
                    }
                }
                if loops.len() > 1 {
                    let mut min = 0;
                    for (idx, l) in loops.iter().enumerate() {
                        if l.1 < loops[min].1 {
                            min = idx;
                        }
                    }
                    for (idx, &(edge, _)) in loops.iter().enumerate() {
                        if idx != min {
                            self.remove(edge);
                        }
                    }
                }
            }
        }
        self.scratch.edges = current_edges;
        self.scratch.loops = loops;
    }

    fn shortest_distance_a_star(&mut self, start: NodeId, end: NodeId, exclude: NodeId, max_length: f64) -> f64 {
        let scratch = &mut self.scratch;
        if scratch.best_stamp.len() < self.nodes.len() {
            scratch.best_stamp.resize(self.nodes.len(), 0);
            scratch.best_distance.resize(self.nodes.len(), f64::INFINITY);
        }
        scratch.stamp = scratch.stamp.wrapping_add(1);
        if scratch.stamp == 0 {
            scratch.best_stamp.fill(0);
            scratch.stamp = 1;
        }
        let stamp = scratch.stamp;
        let frontier = &mut scratch.frontier;
        frontier.clear();
        let nodes = &self.nodes;
        let node_distance =
            |a: NodeId, b: NodeId| distance(&nodes[a as usize].coordinate, &nodes[b as usize].coordinate);
        if exclude != start {
            frontier.push(Candidate { node: start, length: 0.0, min_total_length: node_distance(start, end) });
        }
        while let Some(candidate) = frontier.pop() {
            let current = candidate.node;
            if current == end {
                return candidate.length;
            }
            for &edge in nodes[current as usize].edges.as_slice() {
                let neighbor = self.edges[edge as usize].to;
                if neighbor != exclude {
                    let new_dist = candidate.length + self.pairs[(edge >> 1) as usize].length;
                    let n = neighbor as usize;
                    let prev = if scratch.best_stamp[n] == stamp { scratch.best_distance[n] } else { f64::INFINITY };
                    if new_dist < prev {
                        scratch.best_stamp[n] = stamp;
                        scratch.best_distance[n] = new_dist;
                        let min_total_length = new_dist + node_distance(neighbor, end);
                        if min_total_length <= max_length {
                            frontier.push(Candidate { node: neighbor, length: new_dist, min_total_length });
                        }
                    }
                }
            }
        }
        f64::INFINITY
    }

    fn is_short_stub_edge(&self, edge: EdgeId) -> bool {
        let e = self.edge(edge);
        !e.removed
            && self.length(edge) < self.stub_min_length
            && (self.node(e.from).edges.len() == 1 || self.node(e.to).edges.len() == 1 || e.from == e.to)
    }

    fn remove_short_stub_edges(&mut self) {
        // the queue compares edges by length (which never changes for an edge), so store it alongside the id
        let mut queue: JavaPriorityQueue<(EdgeId, f64), _> =
            JavaPriorityQueue::new(|a: &(EdgeId, f64), b: &(EdgeId, f64)| a.1.total_cmp(&b.1));
        for &node in &self.output {
            for &edge in self.node_edges(node) {
                if self.is_short_stub_edge(edge) {
                    queue.offer((edge, self.length(edge)));
                }
            }
        }
        while !queue.is_empty() {
            let (edge, _) = queue.poll().unwrap();
            if self.edge(edge).removed {
                continue;
            }
            if self.is_short_stub_edge(edge) {
                self.remove(edge);
            }
            let Edge { from, to, .. } = *self.edge(edge);
            for (n, check) in [(from, true), (to, from != to)] {
                if !check {
                    continue;
                }
                if let Some(merged) = self.degree_two_merge(n) {
                    queue.offer((merged, self.length(merged)));
                }
                if self.node(n).edges.len() == 1 {
                    let other = self.node(n).edges.get(0);
                    if self.is_short_stub_edge(other) {
                        queue.offer((other, self.length(other)));
                    }
                }
            }
        }
    }

    fn remove_short_edges(&mut self) {
        let mut edges = std::mem::take(&mut self.scratch.edges);
        for i in 0..self.output.len() {
            let node = self.output[i];
            edges.clear();
            edges.extend_from_slice(self.node_edges(node));
            for &edge in &edges {
                if self.length(edge) < self.min_length {
                    self.remove(edge);
                }
            }
        }
        self.scratch.edges = edges;
    }

    /// Simplifies a main edge (both directions share the coordinates).
    fn simplify_edge(&mut self, edge: EdgeId) {
        debug_assert!(is_main(edge));
        let mut simplified = std::mem::take(&mut self.scratch.coords);
        simplified.clear();
        let coords = self.pair_coords(edge);
        // (changed, may overwrite in place)
        let (changed, in_place) = if let Some(pipeline) = &self.pipeline {
            simplified.extend(pipeline.apply(&Geometry::LineString(LineString(coords.to_vec()))).coords_iter());
            (true, false)
        } else if self.tolerance >= 0.0 {
            douglas_peucker_simplifier::simplify_coords_into(coords, self.tolerance, false, &mut simplified);
            (true, true)
        } else {
            (false, false)
        };
        if changed {
            let pair = &mut self.pairs[(edge >> 1) as usize];
            if in_place {
                // Douglas-Peucker keeps a subset of the points including the first and last, so writing it over the
                // old range never changes the shared split point at the end of the range that the next edge from
                // the same input line starts with.
                debug_assert!(simplified.len() <= pair.len as usize);
                let start = pair.start as usize;
                self.coords[start..start + simplified.len()].copy_from_slice(&simplified);
            } else {
                pair.start = self.coords.len() as u32;
                self.coords.extend_from_slice(&simplified);
            }
            pair.len = simplified.len() as u32;
        }
        self.scratch.coords = simplified;
    }

    fn is_collapsed(&self, edge: EdgeId) -> bool {
        let c = self.pair_coords(edge);
        c.len() < 2 || (c.len() == 2 && c[0] == c[1])
    }

    fn simplify(&mut self) {
        let mut to_remove = std::mem::take(&mut self.scratch.to_remove);
        to_remove.clear();
        for i in 0..self.output.len() {
            let node = self.output[i];
            for j in 0..self.node(node).edges.len() {
                let edge = self.node(node).edges.get(j);
                if is_main(edge) {
                    self.simplify_edge(edge);
                    if self.is_collapsed(edge) {
                        to_remove.push(edge);
                    }
                }
            }
        }
        for &edge in &to_remove {
            self.remove(edge);
        }
        self.scratch.to_remove = to_remove;
    }

    fn remove_duplicated_edges(&mut self) {
        let mut to_remove = std::mem::take(&mut self.scratch.to_remove);
        for i in 0..self.output.len() {
            let node = self.output[i];
            to_remove.clear();
            let edges = self.node_edges(node);
            for (ii, &a) in edges.iter().enumerate() {
                for &b in &edges[ii + 1..] {
                    if self.edge(b).to == self.edge(a).to && self.same_coordinates(a, b) {
                        to_remove.push(b);
                    }
                }
            }
            for &edge in &to_remove {
                self.remove(edge);
            }
        }
        self.scratch.to_remove = to_remove;
    }

    /// Processes the added geometries and returns the merged linestrings. Can be called more than once.
    pub fn get_merged_line_strings(&mut self) -> Vec<LineString<f64>> {
        self.output.clear();
        self.nodes.clear();
        self.edges.clear();
        self.pairs.clear();
        self.coords.clear();
        self.build_graph();

        self.degree_two_merge_all();

        if self.loop_min_length > 0.0 {
            self.break_loops();
            self.degree_two_merge_all();
        }

        if self.stub_min_length > 0.0 {
            self.remove_short_stub_edges();
            // removeShortStubEdges does degreeTwoMerge internally
        }

        if self.pipeline.is_some() || self.tolerance >= 0.0 {
            self.simplify();
            self.remove_duplicated_edges();
            self.degree_two_merge_all();
        }

        if self.merge_strokes {
            self.stroke_merge();
            self.degree_two_merge_all();
        }

        if self.min_length > 0.0 {
            self.remove_short_edges();
        }

        let mut result = Vec::new();
        for &node in &self.output {
            for &edge in self.node_edges(node) {
                if is_main(edge) {
                    result.push(LineString(self.pair_coords(edge).to_vec()));
                }
            }
        }
        result
    }

    /// Snaps the input to the precision grid, splits lines at points shared by more than one line, and builds the
    /// node/edge graph (Java's `nodeLines` + `buildNodes`).
    fn build_graph(&mut self) {
        // snapped coordinates of every input line are stored back-to-back in the coordinate arena, with the
        // "vertex" id (distinct coordinate) of each. Edges are ranges of the arena (consecutive edges of a line
        // share their split point).
        let mut coords = std::mem::take(&mut self.coords);
        coords.reserve(self.input_coords.len());
        let mut vertex_ids: Vec<u32> = Vec::with_capacity(self.input_coords.len());
        let mut lines: Vec<(usize, usize)> = Vec::with_capacity(self.input_ends.len());
        let mut vertex_index: HashMap<(u64, u64), u32> = HashMap::with_capacity(self.input_coords.len());
        // number of times each vertex appears in the input, then the node id of each vertex
        let mut vertices: Vec<(u32, u32)> = Vec::with_capacity(self.input_coords.len());

        let mut line_start = 0;
        for &line_end in &self.input_ends {
            let start = coords.len();
            let mut last: Option<Coord<f64>> = None;
            for c in &self.input_coords[line_start..line_end] {
                let current = self.precision_model.make_precise_coord(*c);
                if last != Some(current) {
                    let next_id = vertices.len() as u32;
                    let id = *vertex_index.entry(coord_key(&current)).or_insert(next_id);
                    if id == next_id {
                        vertices.push((0, NO_NODE));
                    }
                    vertices[id as usize].0 += 1;
                    coords.push(current);
                    vertex_ids.push(id);
                }
                last = Some(current);
            }
            if coords.len() - start >= 2 {
                lines.push((start, coords.len()));
            } else {
                coords.truncate(start);
                vertex_ids.truncate(start);
            }
            line_start = line_end;
        }
        drop(vertex_index);
        self.coords = coords;

        let edge_count_guess = lines.len();
        self.edges.reserve(edge_count_guess * 2);
        self.pairs.reserve(edge_count_guess);
        self.nodes.reserve(edge_count_guess + 1);
        for (start, end) in lines {
            let mut edge_start = start;
            for i in start + 1..end - 1 {
                if vertices[vertex_ids[i] as usize].0 > 1 {
                    self.add_input_edge(&vertex_ids, &mut vertices, edge_start, i + 1);
                    edge_start = i;
                }
            }
            self.add_input_edge(&vertex_ids, &mut vertices, edge_start, end);
        }
    }

    fn get_or_create_node(&mut self, vertices: &mut [(u32, u32)], vertex: u32, coordinate: Coord<f64>) -> NodeId {
        let slot = &mut vertices[vertex as usize].1;
        if *slot == NO_NODE {
            let id = self.nodes.len() as NodeId;
            self.nodes.push(Node { coordinate, edges: EdgeList::new() });
            self.output.push(id);
            *slot = id;
        }
        *slot
    }

    fn add_input_edge(&mut self, vertex_ids: &[u32], vertices: &mut [(u32, u32)], start: usize, end: usize) {
        let (first, last) = (self.coords[start], self.coords[end - 1]);
        let first_node = self.get_or_create_node(vertices, vertex_ids[start], first);
        let last_node = self.get_or_create_node(vertices, vertex_ids[end - 1], last);
        let length = line_length(&self.coords[start..end]);
        let edge = self.new_edge(first_node, last_node, start, end - start, length);
        self.add_edge(first_node, edge);
        if first_node != last_node {
            self.add_edge(last_node, reversed(edge));
        }
    }
}

fn line_length(coords: &[Coord<f64>]) -> f64 {
    coords.windows(2).map(|w| distance(&w[0], &w[1])).sum()
}

/// Minimal port of JTS `org.locationtech.jts.operation.linemerge.LineMerger`: merges line strings that meet at
/// degree-2 nodes, producing the same lines (and directions) as JTS.
pub mod line_merger {
    use std::cmp::Ordering;
    use std::collections::BTreeMap;

    use geo::kernels::{Kernel, Orientation, RobustKernel};
    use geo_types::{Coord, Geometry, LineString};

    struct DirectedEdge {
        to_node: usize,
        edge: usize,
        sym: usize,
        edge_direction: bool,
        quadrant: u8,
        p0: Coord<f64>,
        p1: Coord<f64>,
    }

    struct Node {
        out_edges: Vec<usize>,
        sorted: bool,
        marked: bool,
    }

    #[derive(Clone, Copy)]
    struct Key(f64, f64);

    impl PartialEq for Key {
        fn eq(&self, other: &Self) -> bool {
            self.cmp(other) == Ordering::Equal
        }
    }

    impl Eq for Key {}

    impl PartialOrd for Key {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }

    impl Ord for Key {
        fn cmp(&self, other: &Self) -> Ordering {
            self.0.total_cmp(&other.0).then(self.1.total_cmp(&other.1))
        }
    }

    fn key(c: &Coord<f64>) -> Key {
        Key(if c.x == 0.0 { 0.0 } else { c.x }, if c.y == 0.0 { 0.0 } else { c.y })
    }

    fn quadrant(dx: f64, dy: f64) -> u8 {
        if dx >= 0.0 {
            if dy >= 0.0 { 0 } else { 3 }
        } else if dy >= 0.0 {
            1
        } else {
            2
        }
    }

    /// Merges connected line strings that meet at nodes of degree 2.
    #[derive(Default)]
    pub struct LineMerger {
        lines: Vec<LineString<f64>>,
        nodes: Vec<Node>,
        node_index: BTreeMap<Key, usize>,
        dir_edges: Vec<DirectedEdge>,
        edge_marked: Vec<bool>,
    }

    impl LineMerger {
        pub fn new() -> Self {
            Self::default()
        }

        /// Adds all line string components of `geom`.
        pub fn add(&mut self, geom: &Geometry<f64>) {
            match geom {
                Geometry::LineString(ls) => self.add_line_string(ls),
                Geometry::MultiLineString(mls) => mls.0.iter().for_each(|l| self.add_line_string(l)),
                Geometry::Polygon(p) => {
                    self.add_line_string(p.exterior());
                    p.interiors().iter().for_each(|l| self.add_line_string(l));
                }
                Geometry::MultiPolygon(mp) => {
                    for p in &mp.0 {
                        self.add(&Geometry::Polygon(p.clone()));
                    }
                }
                Geometry::GeometryCollection(gc) => gc.0.iter().for_each(|g| self.add(g)),
                _ => {}
            }
        }

        fn node(&mut self, c: &Coord<f64>) -> usize {
            let next = self.nodes.len();
            let id = *self.node_index.entry(key(c)).or_insert(next);
            if id == next {
                self.nodes.push(Node { out_edges: Vec::new(), sorted: true, marked: false });
            }
            id
        }

        pub fn add_line_string(&mut self, line: &LineString<f64>) {
            let mut coords: Vec<Coord<f64>> = Vec::with_capacity(line.0.len());
            for c in &line.0 {
                if coords.last() != Some(c) {
                    coords.push(*c);
                }
            }
            if coords.len() <= 1 {
                return;
            }
            let edge = self.lines.len();
            self.lines.push(line.clone());
            self.edge_marked.push(false);
            let start = coords[0];
            let end = coords[coords.len() - 1];
            let start_node = self.node(&start);
            let end_node = self.node(&end);
            let d0 = self.dir_edges.len();
            let make = |from: Coord<f64>, to_pt: Coord<f64>, to_node, dir, sym| DirectedEdge {
                to_node,
                edge,
                sym,
                edge_direction: dir,
                quadrant: quadrant(to_pt.x - from.x, to_pt.y - from.y),
                p0: from,
                p1: to_pt,
            };
            self.dir_edges.push(make(start, coords[1], end_node, true, d0 + 1));
            self.dir_edges.push(make(end, coords[coords.len() - 2], start_node, false, d0));
            self.nodes[start_node].out_edges.push(d0);
            self.nodes[start_node].sorted = false;
            self.nodes[end_node].out_edges.push(d0 + 1);
            self.nodes[end_node].sorted = false;
        }

        fn compare_direction(&self, a: usize, b: usize) -> Ordering {
            let (ea, eb) = (&self.dir_edges[a], &self.dir_edges[b]);
            if ea.quadrant != eb.quadrant {
                return ea.quadrant.cmp(&eb.quadrant);
            }
            // JTS Orientation.index(e.p0, e.p1, p1)
            match RobustKernel::orient2d(eb.p0, eb.p1, ea.p1) {
                Orientation::CounterClockwise => Ordering::Greater,
                Orientation::Clockwise => Ordering::Less,
                Orientation::Collinear => Ordering::Equal,
            }
        }

        fn sort_node(&mut self, node: usize) {
            if !self.nodes[node].sorted {
                let mut edges = std::mem::take(&mut self.nodes[node].out_edges);
                edges.sort_by(|a, b| self.compare_direction(*a, *b));
                self.nodes[node].out_edges = edges;
                self.nodes[node].sorted = true;
            }
        }

        fn next(&self, de: usize) -> Option<usize> {
            let to = self.dir_edges[de].to_node;
            let out = &self.nodes[to].out_edges;
            if out.len() != 2 {
                return None;
            }
            if out[0] == self.dir_edges[de].sym { Some(out[1]) } else { Some(out[0]) }
        }

        fn build_edge_string_starting_with(&mut self, start: usize) -> LineString<f64> {
            let mut coords: Vec<Coord<f64>> = Vec::new();
            let (mut forward, mut reverse) = (0, 0);
            let mut current = start;
            loop {
                let de = &self.dir_edges[current];
                let line = &self.lines[de.edge];
                let iter: Box<dyn Iterator<Item = &Coord<f64>>> =
                    if de.edge_direction { Box::new(line.0.iter()) } else { Box::new(line.0.iter().rev()) };
                if de.edge_direction {
                    forward += 1;
                } else {
                    reverse += 1;
                }
                for c in iter {
                    if coords.last() != Some(c) {
                        coords.push(*c);
                    }
                }
                self.edge_marked[de.edge] = true;
                match self.next(current) {
                    Some(n) if n != start => current = n,
                    _ => break,
                }
            }
            if reverse > forward {
                coords.reverse();
            }
            LineString(coords)
        }

        fn build_edge_strings_starting_at(&mut self, node: usize, out: &mut Vec<LineString<f64>>) {
            let edges = self.nodes[node].out_edges.clone();
            for de in edges {
                if self.edge_marked[self.dir_edges[de].edge] {
                    continue;
                }
                out.push(self.build_edge_string_starting_with(de));
            }
        }

        /// Returns the merged line strings.
        pub fn get_merged_line_strings(&mut self) -> Vec<LineString<f64>> {
            for n in 0..self.nodes.len() {
                self.sort_node(n);
                self.nodes[n].marked = false;
            }
            self.edge_marked.iter_mut().for_each(|m| *m = false);
            let order: Vec<usize> = self.node_index.values().copied().collect();
            let mut result = Vec::new();
            for &node in &order {
                if self.nodes[node].out_edges.len() != 2 {
                    self.build_edge_strings_starting_at(node, &mut result);
                    self.nodes[node].marked = true;
                }
            }
            for &node in &order {
                if !self.nodes[node].marked {
                    self.build_edge_strings_starting_at(node, &mut result);
                    self.nodes[node].marked = true;
                }
            }
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::geometry_pipeline;
    use crate::geo::test_utils::new_line_string;
    use geo::{Euclidean, Length};
    use std::collections::HashSet;

    fn ls(c: &[f64]) -> LineString<f64> {
        new_line_string(c)
    }

    fn g(c: &[f64]) -> Geometry<f64> {
        Geometry::LineString(ls(c))
    }

    #[test]
    fn merge_touching_linestrings() {
        let mut merger = LoopLineMerger::new();
        merger.set_min_length(-1.0).set_stub_min_length(-1.0).set_tolerance(-1.0).set_loop_min_length(-1.0);
        merger.add(&g(&[10.0, 10.0, 20.0, 20.0]));
        merger.add(&g(&[20.0, 20.0, 30.0, 30.0]));
        assert_eq!(vec![ls(&[10.0, 10.0, 20.0, 20.0, 30.0, 30.0])], merger.get_merged_line_strings());
    }

    #[test]
    fn keep_two_separate_linestring() {
        let mut merger = LoopLineMerger::new();
        merger.set_min_length(-1.0).set_loop_min_length(-1.0);
        merger.add(&g(&[10.0, 10.0, 20.0, 20.0]));
        merger.add(&g(&[30.0, 30.0, 40.0, 40.0]));
        assert_eq!(
            vec![ls(&[10.0, 10.0, 20.0, 20.0]), ls(&[30.0, 30.0, 40.0, 40.0])],
            merger.get_merged_line_strings()
        );
    }

    #[test]
    fn does_not_overcount_already_added_lines() {
        let mut merger = LoopLineMerger::new();
        merger.set_min_length(-1.0).set_tolerance(-1.0).set_stub_min_length(-1.0).set_loop_min_length(-1.0);
        merger.add(&g(&[10.0, 10.0, 20.0, 20.0]));
        merger.add(&g(&[20.0, 20.0, 30.0, 30.0]));
        merger.add(&g(&[20.0, 20.0, 30.0, 30.0]));
        assert_eq!(vec![ls(&[10.0, 10.0, 20.0, 20.0, 30.0, 30.0])], merger.get_merged_line_strings());
    }

    #[test]
    fn split_linestrings_before_merging() {
        let mut merger = LoopLineMerger::new();
        merger.set_min_length(-1.0).set_loop_min_length(-1.0).set_stub_min_length(-1.0).set_tolerance(-1.0);
        merger.add(&g(&[10.0, 10.0, 20.0, 20.0, 30.0, 30.0]));
        merger.add(&g(&[20.0, 20.0, 30.0, 30.0, 40.0, 40.0]));
        assert_eq!(vec![ls(&[10.0, 10.0, 20.0, 20.0, 30.0, 30.0, 40.0, 40.0])], merger.get_merged_line_strings());
    }

    #[test]
    fn progressive_stub_removal() {
        let mut merger = LoopLineMerger::new();
        merger.set_min_length(-1.0).set_stub_min_length(4.0).set_loop_min_length(-1.0).set_tolerance(-1.0);
        merger.add(&g(&[0.0, 0.0, 5.0, 0.0])); // stub length 5
        merger.add(&g(&[5.0, 0.0, 6.0, 0.0])); // mid piece
        merger.add(&g(&[6.0, 0.0, 8.0, 0.0])); // stub length 2
        merger.add(&g(&[5.0, 0.0, 5.0, 1.0])); // stub length 1
        merger.add(&g(&[6.0, 0.0, 6.0, 1.0])); // stub length 1
        assert_eq!(vec![ls(&[0.0, 0.0, 5.0, 0.0, 6.0, 0.0, 8.0, 0.0])], merger.get_merged_line_strings());
    }

    #[test]
    fn round_coordinates_before_merging() {
        let mut merger = LoopLineMerger::new();
        merger.set_min_length(-1.0).set_loop_min_length(-1.0).set_stub_min_length(-1.0).set_tolerance(-1.0);
        merger.add(&g(&[10.00043983098, 10.0, 20.0, 20.0]));
        merger.add(&g(&[20.0, 20.0, 30.0, 30.0]));
        assert_eq!(vec![ls(&[10.0, 10.0, 20.0, 20.0, 30.0, 30.0])], merger.get_merged_line_strings());
    }

    #[test]
    fn remove_small_loops() {
        let mut merger = LoopLineMerger::new();
        merger.set_min_length(-1.0).set_stub_min_length(-1.0).set_tolerance(-1.0).set_loop_min_length(100.0);
        merger.add(&g(&[10.0, 10.0, 20.0, 10.0, 30.0, 10.0, 30.0, 20.0, 40.0, 20.0]));
        merger.add(&g(&[20.0, 10.0, 30.0, 20.0]));
        assert_eq!(vec![ls(&[10.0, 10.0, 20.0, 10.0, 30.0, 20.0, 40.0, 20.0])], merger.get_merged_line_strings());
    }

    #[test]
    fn remove_self_closing_loops() {
        // Note that self-closing loops are considered stubs.
        // They are removed by stubMinLength, not loopMinLength...
        let mut merger = LoopLineMerger::new();
        merger.set_min_length(-1.0).set_tolerance(-1.0).set_stub_min_length(5.0).set_loop_min_length(-1.0);
        merger.add(&g(&[1.0, -10.0, 1.0, 1.0, 1.0, 2.0, 0.0, 2.0, 0.0, 1.0, 1.0, 1.0, 10.0, 1.0]));
        assert_eq!(vec![ls(&[1.0, -10.0, 1.0, 1.0, 10.0, 1.0])], merger.get_merged_line_strings());
    }

    #[test]
    fn do_not_remove_large_loops() {
        let mut merger = LoopLineMerger::new();
        merger.set_min_length(-1.0).set_loop_min_length(0.001);
        merger.add(&g(&[10.0, 10.0, 20.0, 10.0, 30.0, 10.0, 30.0, 20.0, 40.0, 20.0]));
        merger.add(&g(&[20.0, 10.0, 30.0, 20.0]));
        assert_eq!(
            vec![
                ls(&[10.0, 10.0, 20.0, 10.0]),
                ls(&[20.0, 10.0, 30.0, 10.0, 30.0, 20.0]),
                ls(&[20.0, 10.0, 30.0, 20.0]),
                ls(&[30.0, 20.0, 40.0, 20.0]),
            ],
            merger.get_merged_line_strings()
        );
    }

    #[test]
    fn remove_short_line() {
        let mut merger = LoopLineMerger::new();
        merger.set_min_length(10.0).set_stub_min_length(-1.0).set_tolerance(-1.0).set_loop_min_length(-1.0);
        merger.add(&g(&[10.0, 10.0, 11.0, 11.0]));
        merger.add(&g(&[20.0, 20.0, 30.0, 30.0]));
        merger.add(&g(&[30.0, 30.0, 40.0, 40.0]));
        assert_eq!(vec![ls(&[20.0, 20.0, 30.0, 30.0, 40.0, 40.0])], merger.get_merged_line_strings());
    }

    #[test]
    fn removes_short_stubs_the_non_stubs_that_are_too_short() {
        let mut merger = LoopLineMerger::new();
        merger.set_min_length(0.0).set_loop_min_length(-1.0).set_stub_min_length(15.0).set_tolerance(-1.0);
        merger.add(&g(&[0.0, 0.0, 20.0, 0.0]));
        merger.add(&g(&[20.0, 0.0, 30.0, 0.0]));
        merger.add(&g(&[30.0, 0.0, 50.0, 0.0]));
        merger.add(&g(&[20.0, 0.0, 20.0, 10.0]));
        merger.add(&g(&[30.0, 0.0, 30.0, 10.0]));
        assert_eq!(vec![ls(&[0.0, 0.0, 20.0, 0.0, 30.0, 0.0, 50.0, 0.0])], merger.get_merged_line_strings());
    }

    #[test]
    fn merge_carriageways_with_one_split_shorter_than_loop_min_length() {
        let mut merger = LoopLineMerger::new();
        merger.set_min_length(20.0).set_merge_strokes(true).set_loop_min_length(20.0);
        merger.add(&g(&[0.0, 0.0, 10.0, 0.0, 20.0, 0.0, 30.0, 0.0]));
        merger.add(&g(&[30.0, 0.0, 20.0, 0.0, 15.0, 1.0, 10.0, 0.0, 0.0, 0.0]));
        assert_eq!(vec![ls(&[0.0, 0.0, 10.0, 0.0, 20.0, 0.0, 30.0, 0.0])], merger.get_merged_line_strings());
    }

    #[test]
    fn merge_carriageways_with_one_split_longer_than_loop_min_length() {
        let mut merger = LoopLineMerger::new();
        merger.set_min_length(5.0).set_merge_strokes(true).set_loop_min_length(5.0);
        merger.add(&g(&[0.0, 0.0, 10.0, 0.0, 20.0, 0.0, 30.0, 0.0]));
        merger.add(&g(&[30.0, 0.0, 20.0, 0.0, 15.0, 1.0, 10.0, 0.0, 0.0, 0.0]));
        assert_eq!(
            // ideally loop merging should connect long line strings and represent loops as separate segments off of
            // the edges
            vec![ls(&[0.0, 0.0, 10.0, 0.0, 20.0, 0.0, 30.0, 0.0]), ls(&[20.0, 0.0, 15.0, 1.0, 10.0, 0.0])],
            merger.get_merged_line_strings()
        );
    }

    #[test]
    fn merge_carriageways_with_two_splits() {
        let mut merger = LoopLineMerger::new();
        merger.set_min_length(20.0).set_merge_strokes(true).set_loop_min_length(20.0);
        merger.add(&g(&[0.0, 0.0, 10.0, 0.0, 20.0, 0.0, 30.0, 0.0, 40.0, 0.0]));
        merger.add(&g(&[40.0, 0.0, 30.0, 0.0, 25.0, 5.0, 20.0, 0.0, 15.0, 5.0, 10.0, 0.0, 0.0, 0.0]));
        assert_eq!(vec![ls(&[0.0, 0.0, 10.0, 0.0, 20.0, 0.0, 30.0, 0.0, 40.0, 0.0])], merger.get_merged_line_strings());
    }

    #[test]
    fn merge_loop_attached_to_stub() {
        let mut merger = LoopLineMerger::new();
        merger.set_min_length(10.0).set_loop_min_length(10.0).set_stub_min_length(10.0).set_tolerance(-1.0);
        merger.add(&g(&[-20.0, 0.0, 0.0, 0.0, 20.0, 0.0]));
        merger.add(&g(&[0.0, 0.0, 0.0, 1.0]));
        merger.add(&g(&[0.0, 1.0, 1.0, 2.0, 1.0, 1.0, 0.0, 1.0]));
        assert_eq!(vec![ls(&[-20.0, 0.0, 0.0, 0.0, 20.0, 0.0])], merger.get_merged_line_strings());
    }

    #[test]
    fn real_world_harkingen() {
        let mut merger = LoopLineMerger::new();
        merger.set_min_length(4.0 * 0.0625).set_loop_min_length(8.0 * 0.0625);
        merger.add(&g(&[
            99.185791015625,
            109.83056640625,
            99.202392578125,
            109.8193359375,
            99.21337890625,
            109.810302734375,
            99.222412109375,
            109.8017578125,
            99.229736328125,
            109.793701171875,
            99.241943359375,
            109.779541015625,
        ]));
        merger.add(&g(&[
            98.9931640625,
            109.863525390625,
            99.005126953125,
            109.862060546875,
            99.01708984375,
            109.86083984375,
            99.028564453125,
            109.85986328125,
            99.040283203125,
            109.859375,
            99.0712890625,
            109.85791015625,
            99.08203125,
            109.857421875,
            99.093017578125,
            109.856689453125,
            99.104248046875,
            109.855712890625,
            99.115478515625,
            109.8544921875,
            99.12646484375,
            109.852783203125,
            99.1376953125,
            109.850341796875,
            99.1474609375,
            109.84765625,
            99.15673828125,
            109.844482421875,
            99.166748046875,
            109.84033203125,
            99.175537109375,
            109.836181640625,
            99.185791015625,
            109.83056640625,
        ]));
        merger.add(&g(&[
            99.162841796875,
            109.812744140625,
            99.0966796875,
            109.824462890625,
            99.055419921875,
            109.832275390625,
            99.008544921875,
            109.842041015625,
            98.967529296875,
            109.8525390625,
            98.8818359375,
            109.875244140625,
        ]));
        merger.add(&g(&[
            98.879150390625,
            109.885498046875,
            98.94091796875,
            109.86572265625,
            98.968017578125,
            109.859130859375,
            99.017578125,
            109.847412109375,
            99.056396484375,
            109.83984375,
            99.09814453125,
            109.831298828125,
            99.163330078125,
            109.81982421875,
        ]));
        assert_eq!(1, merger.get_merged_line_strings().len());
    }

    /// Minimal WKB reader supporting the geometry types in the test resources.
    pub(crate) fn read_wkb(bytes: &[u8]) -> Geometry<f64> {
        fn read(b: &[u8], pos: &mut usize) -> Geometry<f64> {
            let le = b[*pos] == 1;
            *pos += 1;
            let u32_at = |b: &[u8], pos: &mut usize| {
                let v: [u8; 4] = b[*pos..*pos + 4].try_into().unwrap();
                *pos += 4;
                if le { u32::from_le_bytes(v) } else { u32::from_be_bytes(v) }
            };
            let f64_at = |b: &[u8], pos: &mut usize| {
                let v: [u8; 8] = b[*pos..*pos + 8].try_into().unwrap();
                *pos += 8;
                if le { f64::from_le_bytes(v) } else { f64::from_be_bytes(v) }
            };
            let typ = u32_at(b, pos);
            let coords = |b: &[u8], pos: &mut usize| {
                let n = u32_at(b, pos) as usize;
                (0..n).map(|_| Coord { x: f64_at(b, pos), y: f64_at(b, pos) }).collect::<Vec<_>>()
            };
            match typ {
                1 => {
                    let x = f64_at(b, pos);
                    let y = f64_at(b, pos);
                    Geometry::Point(geo_types::Point::new(x, y))
                }
                2 => Geometry::LineString(LineString(coords(b, pos))),
                3 => {
                    let n = u32_at(b, pos) as usize;
                    let mut rings: Vec<LineString<f64>> = (0..n).map(|_| LineString(coords(b, pos))).collect();
                    let ext = if rings.is_empty() { LineString(vec![]) } else { rings.remove(0) };
                    Geometry::Polygon(geo_types::Polygon::new(ext, rings))
                }
                4..=7 => {
                    let n = u32_at(b, pos) as usize;
                    let parts: Vec<Geometry<f64>> = (0..n).map(|_| read(b, pos)).collect();
                    Geometry::GeometryCollection(geo_types::GeometryCollection(parts))
                }
                other => panic!("unsupported wkb type {other}"),
            }
        }
        let mut pos = 0;
        read(bytes, &mut pos)
    }

    pub(crate) fn read_resource(file: &str) -> Geometry<f64> {
        use std::io::Read;
        let local = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/test/resources/mergelines").join(file);
        let path = if local.exists() {
            local
        } else {
            std::path::Path::new(
                "/Users/ian.dees/Workspace/planetiler-ref/planetiler/planetiler-core/src/test/resources/mergelines",
            )
            .join(file)
        };
        let compressed = std::fs::read(&path).unwrap_or_else(|e| panic!("reading {path:?}: {e}"));
        let mut bytes = Vec::new();
        flate2::read::GzDecoder::new(&compressed[..]).read_to_end(&mut bytes).unwrap();
        read_wkb(&bytes)
    }

    fn on_real_world_data(file: &str, min_lengths: f64, simplify: bool, expected: usize) {
        let geom = read_resource(file);
        let mut merger = LoopLineMerger::new();
        merger.set_min_length(min_lengths);
        merger.set_loop_min_length(min_lengths);
        merger.set_stub_min_length(min_lengths);
        merger.set_merge_strokes(true);
        merger.set_tolerance(if simplify { 1.0 } else { -1.0 });
        merger.add(&geom);
        let merged = merger.get_merged_line_strings();
        let mut lines: HashSet<Vec<(u64, u64)>> = HashSet::new();
        let mut merger2 = line_merger::LineMerger::new();
        for line in &merged {
            merger2.add(&Geometry::LineString(line.clone()));
            assert!(lines.insert(line.0.iter().map(coord_key).collect()), "contained duplicate: {line:?}");
            if min_lengths > 0.0 && !simplify {
                // simplification can make an edge < min length
                assert!(Euclidean.length(line) >= min_lengths, "line < {min_lengths}: {line:?}");
            }
        }
        // ensure there are no more opportunities for simplification found by JTS:
        let jts = merger2.get_merged_line_strings();
        let missing: Vec<_> = jts.iter().filter(|l| !merged.contains(l)).collect();
        let extra: Vec<_> = merged.iter().filter(|l| !jts.contains(l)).collect();
        assert!(missing.is_empty(), "{file} missing edges: {missing:?}");
        assert!(extra.is_empty(), "{file} extra edges: {extra:?}");
        assert_eq!(merged.len(), jts.len());
        assert_eq!(expected, merged.len(), "{file},{min_lengths},{simplify}");
    }

    #[test]
    fn on_real_world_data_cases() {
        for (file, min, simplify, expected) in [
            ("mergelines_1759_point_line.wkb.gz", 0.0, false, 3),
            ("mergelines_1759_point_line.wkb.gz", 1.0, false, 2),
            ("mergelines_1759_point_line.wkb.gz", 1.0, true, 2),
            ("mergelines_200433_lines.wkb.gz", 0.0, false, 9103),
            ("mergelines_200433_lines.wkb.gz", 0.1, false, 8834),
            ("mergelines_200433_lines.wkb.gz", 1.0, false, 861),
            ("mergelines_200433_lines.wkb.gz", 1.0, true, 508),
            ("mergelines_239823_lines.wkb.gz", 0.0, false, 6188),
            ("mergelines_239823_lines.wkb.gz", 0.1, false, 5941),
            ("mergelines_239823_lines.wkb.gz", 1.0, false, 826),
            ("mergelines_239823_lines.wkb.gz", 1.0, true, 681),
            ("i90.wkb.gz", 0.0, false, 17),
            ("i90.wkb.gz", 1.0, false, 18),
            ("i90.wkb.gz", 20.0, false, 4),
            ("i90.wkb.gz", 30.0, false, 1),
        ] {
            on_real_world_data(file, min, simplify, expected);
        }
    }

    fn stroke_merger() -> LoopLineMerger {
        let mut merger = LoopLineMerger::new();
        merger.set_min_length(1.0).set_loop_min_length(1.0).set_stub_min_length(1.0).set_merge_strokes(true);
        merger
    }

    #[test]
    fn merge_strokes_at_3_way_intersection_with_loop() {
        let mut merger = stroke_merger();
        merger.add(&g(&[-5.0, 0.0, 0.0, 0.0]));
        merger.add(&g(&[0.0, 0.0, 5.0, 0.0, 5.0, 5.0, 0.0, 5.0, 0.0, 0.0]));
        assert_eq!(
            vec![ls(&[-5.0, 0.0, 0.0, 0.0, 5.0, 0.0, 5.0, 5.0, 0.0, 5.0, 0.0, 0.0])],
            merger.get_merged_line_strings()
        );
    }

    #[test]
    fn merge_strokes_at_3_way_intersection_with_loop2() {
        let mut merger = stroke_merger();
        merger.add(&g(&[-5.0, 0.0, 0.0, 0.0]));
        merger.add(&g(&[0.0, 0.0, 0.0, -1.0, 5.0, 0.0, 5.0, 5.0, 0.0, 5.0, 0.0, 0.0]));
        assert_eq!(
            vec![ls(&[-5.0, 0.0, 0.0, 0.0, 0.0, -1.0, 5.0, 0.0, 5.0, 5.0, 0.0, 5.0, 0.0, 0.0])],
            merger.get_merged_line_strings()
        );
    }

    #[test]
    fn merge_strokes_at_3_way_intersection() {
        let mut merger = stroke_merger();
        merger.add(&g(&[-5.0, 0.0, 0.0, 0.0]));
        merger.add(&g(&[0.0, 0.0, 5.0, 0.0]));
        merger.add(&g(&[0.0, 0.0, 0.0, 5.0]));
        assert_eq!(
            vec![ls(&[-5.0, 0.0, 0.0, 0.0, 5.0, 0.0]), ls(&[0.0, 0.0, 0.0, 5.0])],
            merger.get_merged_line_strings()
        );
    }

    #[test]
    fn simplify_tolerance() {
        let mut merger = LoopLineMerger::new();
        merger.set_tolerance(1.0);
        merger.add(&g(&[0.0, 0.0, 5.0, 1.0]));
        merger.add(&g(&[5.0, 1.0, 10.0, 0.0]));
        assert_eq!(vec![ls(&[0.0, 0.0, 10.0, 0.0])], merger.get_merged_line_strings());
    }

    #[test]
    fn geometry_pipeline() {
        let mut merger = LoopLineMerger::new();
        merger.set_segment_transform(std::sync::Arc::new(geometry_pipeline::simplify_dp(1.0)));
        merger.add(&g(&[0.0, 0.0, 5.0, 1.0]));
        merger.add(&g(&[5.0, 1.0, 10.0, 0.0]));
        assert_eq!(vec![ls(&[0.0, 0.0, 10.0, 0.0])], merger.get_merged_line_strings());
    }

    #[test]
    fn merge_strokes_at_4_way_intersection() {
        let mut merger = stroke_merger();
        merger.add(&g(&[-5.0, 0.0, 0.0, 0.0]));
        merger.add(&g(&[0.0, 0.0, 5.0, 0.0]));
        merger.add(&g(&[0.0, 0.0, 0.0, 5.0]));
        merger.add(&g(&[0.0, 0.0, 0.0, -5.0]));
        assert_eq!(
            vec![ls(&[-5.0, 0.0, 0.0, 0.0, 5.0, 0.0]), ls(&[0.0, -5.0, 0.0, 0.0, 0.0, 5.0])],
            merger.get_merged_line_strings()
        );
    }

    #[test]
    fn merge_strokes_at_5_way_intersection() {
        let mut merger = stroke_merger();
        merger.add(&g(&[-5.0, 0.0, 0.0, 0.0]));
        merger.add(&g(&[0.0, 0.0, 5.0, 0.0]));
        merger.add(&g(&[0.0, 0.0, 0.0, 5.0]));
        merger.add(&g(&[0.0, 0.0, 0.0, -5.0]));
        merger.add(&g(&[0.0, 0.0, 5.0, 5.0]));
        assert_eq!(
            vec![ls(&[-5.0, 0.0, 0.0, 0.0, 5.0, 0.0]), ls(&[0.0, 0.0, 5.0, 5.0]), ls(&[0.0, -5.0, 0.0, 0.0, 0.0, 5.0])],
            merger.get_merged_line_strings()
        );
    }
}

//! Port of JTS `CascadedPolygonUnion` (what `Geometry.union()` uses for a collection of polygons), including the
//! `STRtree` (node capacity 4) item order that determines which polygons are unioned together.
//!
//! The union of many polygons is computed by unioning nearby pairs, then pairs of the results, and so on. Each
//! intersection point between the edges of a pair is computed from the segments of that pair (see
//! [`polygon_overlay::union_pair`]), and later levels intersect segments that end at those points, so the exact
//! output vertices depend on the order. Matching JTS's order gives the same vertices as Java's
//! `geometryCollection.union()` (up to the overlay engine's topology decisions), which matters wherever a later
//! step rounds them (e.g. a vertex exactly half a tile pixel unit from the grid).

use geo_types::{Geometry, Polygon};

use crate::geo::geometry_exception::GeometryResult;
use crate::geo::polygon_overlay;

const STRTREE_NODE_CAPACITY: usize = 4;

#[derive(Clone, Copy, Debug)]
struct Env {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

impl Env {
    fn of(poly: &Polygon<f64>) -> Env {
        let mut env =
            Env { min_x: f64::INFINITY, min_y: f64::INFINITY, max_x: f64::NEG_INFINITY, max_y: f64::NEG_INFINITY };
        for c in &poly.exterior().0 {
            env.min_x = env.min_x.min(c.x);
            env.min_y = env.min_y.min(c.y);
            env.max_x = env.max_x.max(c.x);
            env.max_y = env.max_y.max(c.y);
        }
        env
    }

    fn expand(&mut self, other: &Env) {
        self.min_x = self.min_x.min(other.min_x);
        self.min_y = self.min_y.min(other.min_y);
        self.max_x = self.max_x.max(other.max_x);
        self.max_y = self.max_y.max(other.max_y);
    }

    fn centre_x(&self) -> f64 {
        (self.min_x + self.max_x) / 2.0
    }

    fn centre_y(&self) -> f64 {
        (self.min_y + self.max_y) / 2.0
    }
}

enum Node {
    Item(usize, Env),
    Branch(Vec<Node>, Env),
}

impl Node {
    fn env(&self) -> &Env {
        match self {
            Node::Item(_, env) | Node::Branch(_, env) => env,
        }
    }
}

/// JTS `AbstractSTRtree.compareDoubles` (NaN compares equal).
fn compare_doubles(a: f64, b: f64) -> std::cmp::Ordering {
    a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)
}

fn branch(children: Vec<Node>) -> Node {
    let mut env = *children[0].env();
    for c in &children[1..] {
        env.expand(c.env());
    }
    Node::Branch(children, env)
}

/// `STRtree.createParentBoundables`: sort by x into vertical slices, then each slice by y into nodes.
fn create_parent_boundables(mut children: Vec<Node>) -> Vec<Node> {
    let n = children.len();
    let min_leaf_count = (n as f64 / STRTREE_NODE_CAPACITY as f64).ceil() as usize;
    // stable sorts, like Java's Collections.sort
    children.sort_by(|a, b| compare_doubles(a.env().centre_x(), b.env().centre_x()));
    let slice_count = (min_leaf_count as f64).sqrt().ceil() as usize;
    let slice_capacity = (n as f64 / slice_count as f64).ceil() as usize;
    let mut parents = Vec::new();
    let mut iter = children.into_iter();
    for _ in 0..slice_count {
        let mut slice: Vec<Node> = iter.by_ref().take(slice_capacity).collect();
        if slice.is_empty() {
            continue;
        }
        slice.sort_by(|a, b| compare_doubles(a.env().centre_y(), b.env().centre_y()));
        let mut iter = slice.into_iter().peekable();
        while iter.peek().is_some() {
            parents.push(branch(iter.by_ref().take(STRTREE_NODE_CAPACITY).collect()));
        }
    }
    parents
}

fn build_tree(items: Vec<Node>) -> Node {
    let mut level = items;
    loop {
        let mut parents = create_parent_boundables(level);
        if parents.len() == 1 {
            return parents.pop().unwrap();
        }
        level = parents;
    }
}

struct Union {
    polys: Vec<Option<Polygon<f64>>>,
}

type Part = Option<Vec<Polygon<f64>>>;

impl Union {
    fn union_tree(&mut self, children: Vec<Node>) -> GeometryResult<Part> {
        let mut geoms: Vec<Part> = Vec::with_capacity(children.len());
        for child in children {
            geoms.push(match child {
                Node::Item(i, _) => Some(vec![self.polys[i].take().expect("each item once")]),
                Node::Branch(children, _) => self.union_tree(children)?,
            });
        }
        let end = geoms.len();
        binary_union(&mut geoms, 0, end)
    }
}

fn binary_union(geoms: &mut [Part], start: usize, end: usize) -> GeometryResult<Part> {
    if end - start <= 1 {
        let g0 = geoms.get_mut(start).and_then(Option::take);
        union_safe(g0, None)
    } else if end - start == 2 {
        let g0 = geoms[start].take();
        let g1 = geoms.get_mut(start + 1).and_then(Option::take);
        union_safe(g0, g1)
    } else {
        let mid = (end + start) / 2;
        let g0 = binary_union(geoms, start, mid)?;
        let g1 = binary_union(geoms, mid, end)?;
        union_safe(g0, g1)
    }
}

fn union_safe(g0: Part, g1: Part) -> GeometryResult<Part> {
    Ok(match (g0, g1) {
        (None, None) => None,
        (None, Some(g)) | (Some(g), None) => Some(g),
        (Some(a), Some(b)) => Some(polygon_overlay::union_pair(&a, &b)?),
    })
}

/// JTS `CascadedPolygonUnion.union(polygons)` / `geometryCollection.union()` for polygons (empty polygons are
/// ignored). Returns an empty polygon for no input.
pub fn union(polys: Vec<Polygon<f64>>) -> GeometryResult<Geometry<f64>> {
    let polys: Vec<Polygon<f64>> = polys.into_iter().filter(|p| !p.exterior().0.is_empty()).collect();
    if polys.is_empty() {
        return Ok(polygon_overlay::to_polygonal(vec![]));
    }
    let items: Vec<Node> = polys.iter().enumerate().map(|(i, p)| Node::Item(i, Env::of(p))).collect();
    let Node::Branch(children, _) = build_tree(items) else { unreachable!("root is a branch") };
    let mut state = Union { polys: polys.into_iter().map(Some).collect() };
    let result = state.union_tree(children)?.unwrap_or_default();
    Ok(polygon_overlay::to_polygonal(result))
}

/// [`union`] of the polygons in `geom`.
pub fn union_geometry(geom: &Geometry<f64>) -> GeometryResult<Geometry<f64>> {
    union(polygon_overlay::polygons_of(geom).into_iter().cloned().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::test_utils::rectangle_xy;
    use geo::Area;

    #[test]
    fn union_of_overlapping_rectangles() {
        let polys: Vec<Polygon<f64>> = (0..10).map(|i| rectangle_xy(i as f64, 0.0, i as f64 + 1.5, 1.0)).collect();
        let result = union(polys).unwrap();
        assert!(matches!(result, Geometry::Polygon(_)), "{result:?}");
        assert!((result.unsigned_area() - 10.5).abs() < 1e-9);
    }

    #[test]
    fn union_of_disjoint_rectangles() {
        let polys: Vec<Polygon<f64>> =
            (0..7).map(|i| rectangle_xy(i as f64 * 2.0, 0.0, i as f64 * 2.0 + 1.0, 1.0)).collect();
        let Geometry::MultiPolygon(mp) = union(polys).unwrap() else { panic!() };
        assert_eq!(7, mp.0.len());
    }

    #[test]
    fn single_polygon_is_returned_as_is() {
        let poly = rectangle_xy(0.0, 0.0, 1.0, 1.0);
        assert_eq!(Geometry::Polygon(poly.clone()), union(vec![poly]).unwrap());
        assert_eq!(0.0, union(vec![]).unwrap().unsigned_area());
    }
}

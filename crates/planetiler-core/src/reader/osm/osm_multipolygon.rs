//! Port of `reader/osm/OsmMultipolygon.java`: reconstructs
//! [multipolygons](https://wiki.openstreetmap.org/wiki/Relation:multipolygon) from individual ways that represent the
//! edges of inner and outer rings.
//!
//! Multipolygon way members have an "inner" and "outer" role, but they can be incorrectly specified, so instead
//! determine the nesting order and alternate outer/inner/outer/inner... from the outermost ring inwards.
//!
//! Ported to Java (and from there to Rust) from
//! [imposm3 multipolygon.go](https://github.com/omniscale/imposm3/blob/master/geom/multipolygon.go) and
//! [imposm3 ring.go](https://github.com/omniscale/imposm3/blob/master/geom/ring.go).
//!
//! Steps to reconstruct a polygon:
//! 1. connect ways with matching endpoints until closed rings are formed (discard unclosed rings)
//! 2. sort rings by area descending
//! 3. process rings in order to find out which is the direct parent containing each ring
//! 4. iterate from outermost to innermost ring, creating a polygon with holes for each outer/inner ring pair

use std::collections::{BTreeMap, HashMap};
use std::panic::{AssertUnwindSafe, catch_unwind};

use geo::{Area, PreparedGeometry, Relate};
use geo_types::{Coord, Geometry, LineString, MultiPolygon, Polygon};
use indexmap::IndexSet;

use crate::collection::long_long_map::{LongLongMap, MISSING_VALUE};
use crate::geo::envelope::Envelope;
use crate::geo::geo_utils;
use crate::geo::geometry_exception::{GeometryException, GeometryResult};
use crate::geo::is_valid::{Location, locate_in_ring};

/// Gap (in world coordinates) below which an almost-closed ring gets closed: 10cm at the equator.
pub const MIN_CLOSE_RING_GAP: f64 = 0.1 / geo_utils::WORLD_CIRCUMFERENCE_METERS;

/// Port of `OsmReader.NodeLocationProvider`: looks up the location of OSM nodes by ID.
/// Re-exported as `osm_reader::NodeLocationProvider`; `osm_reader::NodeLocations` implements it.
pub trait NodeLocationProvider {
    /// Returns the coordinate of node `id`, or an error if it is missing (Java throws `IllegalArgumentException`).
    fn get_coordinate(&self, id: i64) -> anyhow::Result<Coord<f64>>;

    /// Returns the coordinates of each node in `node_ids`.
    fn get_way_geometry(&self, node_ids: &[i64]) -> anyhow::Result<Vec<Coord<f64>>> {
        node_ids.iter().map(|&id| self.get_coordinate(id)).collect()
    }
}

impl<F: Fn(i64) -> Option<Coord<f64>>> NodeLocationProvider for F {
    fn get_coordinate(&self, id: i64) -> anyhow::Result<Coord<f64>> {
        self(id).ok_or_else(|| anyhow::anyhow!("Missing location for node: {id}"))
    }
}

/// A [`NodeLocationProvider`] that reads node locations encoded with [`geo_utils::encode_flat_location`] from a
/// [`LongLongMap`] (like `OsmReader.NodeDbLocationProvider`), returning world web mercator coordinates.
pub struct LongLongMapLocationProvider<'a>(pub &'a dyn LongLongMap);

impl NodeLocationProvider for LongLongMapLocationProvider<'_> {
    fn get_coordinate(&self, id: i64) -> anyhow::Result<Coord<f64>> {
        let encoded = self.0.get(id);
        if encoded == MISSING_VALUE {
            anyhow::bail!("Missing location for node: {id}");
        }
        let encoded = encoded as u64;
        Ok(Coord { x: geo_utils::decode_world_x(encoded), y: geo_utils::decode_world_y(encoded) })
    }
}

/// A closed ring that tracks parent and child ring relationships (by index into the list of rings).
struct Ring {
    geom: Polygon<f64>,
    area: f64,
    envelope: Envelope,
    contained_by: Option<usize>,
    // use an insertion-ordered set to ensure same input always produces same output
    holes: IndexSet<usize>,
}

impl Ring {
    fn new(geom: Polygon<f64>) -> Ring {
        let area = geom.unsigned_area();
        let envelope = Envelope::of_geometry(&Geometry::Polygon(geom.clone()));
        Ring { geom, area, envelope, contained_by: None, holes: IndexSet::new() }
    }
}

fn to_polygon(rings: &[Ring], idx: usize) -> Polygon<f64> {
    let ring = &rings[idx];
    Polygon::new(ring.geom.exterior().clone(), ring.holes.iter().map(|&h| rings[h].geom.exterior().clone()).collect())
}

/// Returns true if this is an outermost ring and alternate false/true as you work inwards.
fn is_hole(rings: &[Ring], idx: usize) -> bool {
    let mut counter = 0;
    let mut ring = Some(idx);
    while let Some(i) = ring {
        counter += 1;
        ring = rings[i].contained_by;
    }
    counter % 2 == 0
}

/// Builds a multipolygon from line strings containing the locations of nodes in edge segments.
pub fn build_from_coordinates(rings: &[Vec<Coord<f64>>]) -> GeometryResult<Geometry<f64>> {
    fn key(c: &Coord<f64>) -> (u64, u64) {
        // Java's Coordinate.equals treats -0.0 == 0.0
        ((c.x + 0.0).to_bits(), (c.y + 0.0).to_bits())
    }
    let mut coord_to_id: HashMap<(u64, u64), i64> = HashMap::new();
    let mut id_to_coord: Vec<Coord<f64>> = Vec::new();
    let mut id_rings = Vec::with_capacity(rings.len());
    for coords in rings {
        let mut id_ring = Vec::with_capacity(coords.len());
        for coord in coords {
            let id = *coord_to_id.entry(key(coord)).or_insert_with(|| {
                id_to_coord.push(*coord);
                id_to_coord.len() as i64 - 1
            });
            id_ring.push(id);
        }
        id_rings.push(id_ring);
    }
    let lookup = |id: i64| id_to_coord.get(id as usize).copied();
    build_with_gap(&id_rings, &lookup, 0, 0.0)
}

/// Builds a multipolygon from line strings containing the IDs of nodes in edge segments and a node ID location
/// provider, closing rings whose endpoints are within [`MIN_CLOSE_RING_GAP`] units.
pub fn build(rings: &[Vec<i64>], node_cache: &dyn NodeLocationProvider, osm_id: i64) -> GeometryResult<Geometry<f64>> {
    build_with_gap(rings, node_cache, osm_id, MIN_CLOSE_RING_GAP)
}

/// Builds a multipolygon from line strings containing the IDs of nodes in edge segments and a node ID location
/// provider, closing rings whose endpoints are within `min_gap` units.
pub fn build_with_gap(
    rings: &[Vec<i64>],
    node_cache: &dyn NodeLocationProvider,
    osm_id: i64,
    min_gap: f64,
) -> GeometryResult<Geometry<f64>> {
    do_build(rings, node_cache, osm_id, min_gap, false)
}

/// Error from one build attempt: either a final [`GeometryException`], a "topology exception" (a panic inside the
/// geometry library, which triggers a retry with fixed rings), or any other error.
enum BuildError {
    Geometry(GeometryException),
    Topology(String),
    Other(String),
}

impl From<GeometryException> for BuildError {
    fn from(e: GeometryException) -> Self {
        BuildError::Geometry(e)
    }
}

impl From<anyhow::Error> for BuildError {
    fn from(e: anyhow::Error) -> Self {
        BuildError::Other(format!("{e:#}"))
    }
}

fn do_build(
    rings: &[Vec<i64>],
    node_cache: &dyn NodeLocationProvider,
    osm_id: i64,
    min_gap: f64,
    fix: bool,
) -> GeometryResult<Geometry<f64>> {
    match try_build(rings, node_cache, osm_id, min_gap, fix) {
        Ok(geom) => Ok(geom),
        Err(BuildError::Geometry(e)) => Err(e),
        // retry but fix every polygon first
        Err(BuildError::Topology(_)) if !fix => do_build(rings, node_cache, osm_id, min_gap, true),
        Err(BuildError::Topology(e) | BuildError::Other(e)) => Err(GeometryException::new(
            "osm_invalid_multipolygon",
            format!("error building multipolygon {osm_id}: {e}"),
        )),
    }
}

fn try_build(
    rings: &[Vec<i64>],
    node_cache: &dyn NodeLocationProvider,
    osm_id: i64,
    min_gap: f64,
    fix: bool,
) -> Result<Geometry<f64>, BuildError> {
    if rings.is_empty() {
        return Err(GeometryException::verbose(
            "osm_invalid_multipolygon_empty",
            format!("error building multipolygon {osm_id}: no rings to process"),
        )
        .into());
    }
    let id_segments = connect_polygon_segments(rings);
    let mut polygons = get_closed_rings(node_cache, min_gap, fix, id_segments)?;
    if polygons.is_empty() {
        return Err(GeometryException::verbose(
            "osm_invalid_multipolygon_empty_after_fix",
            format!("error building multipolygon {osm_id}: no rings to process after fixing"),
        )
        .into());
    }
    // stable sort, like Java's List.sort
    polygons.sort_by(|a, b| b.area.total_cmp(&a.area));
    let shells = match catch_unwind(AssertUnwindSafe(|| group_parent_child_shells(&mut polygons))) {
        Ok(shells) => shells,
        Err(panic) => {
            let msg = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            return Err(BuildError::Topology(format!("TopologyException: {msg}")));
        }
    };
    if shells.is_empty() {
        Err(GeometryException::verbose(
            "osm_invalid_multipolygon_not_closed",
            format!("error building multipolygon {osm_id}: multipolygon not closed"),
        )
        .into())
    } else if shells.len() == 1 {
        Ok(Geometry::Polygon(to_polygon(&polygons, shells[0])))
    } else {
        Ok(Geometry::MultiPolygon(MultiPolygon::new(shells.iter().map(|&s| to_polygon(&polygons, s)).collect())))
    }
}

fn get_closed_rings(
    node_cache: &dyn NodeLocationProvider,
    min_gap: f64,
    fix: bool,
    id_segments: Vec<Vec<i64>>,
) -> Result<Vec<Ring>, BuildError> {
    let mut polygons = Vec::with_capacity(id_segments.len());
    for mut segment in id_segments {
        let first_id = segment[0];
        let last_id = segment[segment.len() - 1];
        if first_id == last_id || try_close(&mut segment, node_cache, min_gap)? {
            let coordinates = node_cache.get_way_geometry(&segment)?;
            let poly = Polygon::new(LineString::new(coordinates), vec![]);
            // the first time through, just process the polygon
            // if that fails, then the second time attempt to repair the geometry before processing
            if fix {
                add_polygon_rings(&mut polygons, geo_utils::fix_polygon(&Geometry::Polygon(poly))?);
            } else {
                polygons.push(Ring::new(poly));
            }
        }
    }
    Ok(polygons)
}

fn add_polygon_rings(polygons: &mut Vec<Ring>, geom: Geometry<f64>) {
    match geom {
        Geometry::Polygon(poly) => polygons.push(Ring::new(poly)),
        Geometry::MultiPolygon(mp) => polygons.extend(mp.0.into_iter().map(Ring::new)),
        Geometry::GeometryCollection(gc) => {
            for g in gc.0 {
                add_polygon_rings(polygons, g);
            }
        }
        _ => {}
    }
}

/// Port of JTS `PreparedPolygon.contains` with cheap envelope/point rejections first.
fn contains<'a>(outer: &Ring, prepared: &PreparedGeometry<'a, &'a Polygon<f64>>, inner: &Ring) -> bool {
    if !outer.envelope.contains(&inner.envelope) {
        return false;
    }
    // a vertex of inner strictly outside of the outer shell means it can't be contained
    let shell = &outer.geom.exterior().0;
    if let Some(&first) = inner.geom.exterior().0.first()
        && shell.len() >= 4
        && locate_in_ring(first, shell) == Location::Exterior
    {
        return false;
    }
    prepared.relate(&inner.geom).is_contains()
}

fn group_parent_child_shells(polygons: &mut [Ring]) -> IndexSet<usize> {
    // use insertion-ordered set to ensure the same input always produces the same output
    let mut shells = IndexSet::new();
    let num_polygons = polygons.len();
    if num_polygons == 0 {
        return shells;
    }
    shells.insert(0);
    if num_polygons == 1 {
        return shells;
    }
    for i in 0..num_polygons {
        if i < num_polygons - 1 {
            let outer_geom = polygons[i].geom.clone();
            let prepared = PreparedGeometry::from(&outer_geom);
            // since the rings are sorted by area descending, the inner loop is only smaller rings
            for j in i + 1..num_polygons {
                if contains(&polygons[i], &prepared, &polygons[j]) {
                    // keep searching until we find the smallest ring that contains this one
                    // that one is the direct parent
                    if let Some(parent) = polygons[j].contained_by {
                        polygons[parent].holes.shift_remove(&j);
                        shells.shift_remove(&j);
                    }
                    polygons[j].contained_by = Some(i);
                    if is_hole(polygons, j) {
                        polygons[i].holes.insert(j);
                    } else {
                        shells.insert(j);
                    }
                }
            }
        }
        if polygons[i].contained_by.is_none() {
            shells.insert(i);
        }
    }
    shells
}

fn try_close(segment: &mut [i64], node_cache: &dyn NodeLocationProvider, min_gap: f64) -> anyhow::Result<bool> {
    let size = segment.len();
    let first_id = segment[0];
    let first = node_cache.get_coordinate(first_id)?;
    let last = node_cache.get_coordinate(segment[size - 1])?;
    if (first.x - last.x).hypot(first.y - last.y) <= min_gap {
        segment[size - 1] = first_id;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Appends `to_append` to `orig`, skipping the first element of `to_append`.
pub(crate) fn append_to_skip_first(orig: &mut Vec<i64>, to_append: &[i64]) {
    orig.extend_from_slice(&to_append[1..]);
}

/// Prepends `to_prepend` to `orig`, skipping the last element of `to_prepend`.
pub(crate) fn prepend_to_skip_last(orig: &mut Vec<i64>, to_prepend: &[i64]) {
    orig.splice(0..0, to_prepend[..to_prepend.len() - 1].iter().copied());
}

/// Connects way node ID sequences with matching endpoints, returning the closed rings (or unclosed rings with at least
/// 4 nodes that may get closed by `try_close`).
pub(crate) fn connect_polygon_segments(outer: &[Vec<i64>]) -> Vec<Vec<i64>> {
    // arena of partial rings, each registered under its 2 endpoints in endpoint_index
    let mut arena: Vec<Vec<i64>> = Vec::new();
    let mut endpoint_index: HashMap<i64, usize> = HashMap::with_capacity(outer.len() * 2);
    let mut complete_rings: Vec<Vec<i64>> = Vec::with_capacity(outer.len());

    for ids in outer {
        if ids.len() <= 1 {
            continue;
        }
        let first = ids[0];
        let last = ids[ids.len() - 1];

        // optimization - skip rings that are already closed (as long as they have enough points)
        if first == last {
            if ids.len() >= 4 {
                complete_rings.push(ids.clone());
            }
            continue;
        }

        if let Some(m) = endpoint_index.remove(&first) {
            if first == arena[m][0] {
                arena[m].reverse();
            }
            append_to_skip_first(&mut arena[m], ids);
            match endpoint_index.get(&last).copied() {
                Some(next) if next != m => {
                    endpoint_index.remove(&last);
                    let next_ids = std::mem::take(&mut arena[next]);
                    if last == next_ids[0] {
                        append_to_skip_first(&mut arena[m], &next_ids);
                    } else {
                        let reversed: Vec<i64> = next_ids.iter().rev().copied().collect();
                        append_to_skip_first(&mut arena[m], &reversed);
                    }
                    let new_last = *arena[m].last().unwrap();
                    endpoint_index.insert(new_last, m);
                }
                _ => {
                    endpoint_index.insert(last, m);
                }
            }
        } else if let Some(m) = endpoint_index.remove(&last) {
            if last == arena[m][0] {
                prepend_to_skip_last(&mut arena[m], ids);
            } else {
                let reversed: Vec<i64> = ids.iter().rev().copied().collect();
                append_to_skip_first(&mut arena[m], &reversed);
            }
            endpoint_index.insert(first, m);
        } else {
            arena.push(ids.clone());
            let idx = arena.len() - 1;
            endpoint_index.insert(first, idx);
            endpoint_index.insert(last, idx);
        }
    }

    // iterate in sorted order to ensure the same input always produces the same output
    let sorted: BTreeMap<i64, usize> = endpoint_index.into_iter().collect();
    for (key, idx) in sorted {
        let value = &arena[idx];
        if value.len() >= 4 && (value[0] == value[value.len() - 1] || key == value[0]) {
            complete_rings.push(value.clone());
        }
    }
    complete_rings
}

#[cfg(test)]
mod tests {
    //! Ported from <https://github.com/omniscale/imposm3/blob/master/geom/multipolygon_test.go> via Java.
    use super::*;
    use crate::geo::test_utils::*;

    fn sorted(mut v: Vec<Vec<i64>>) -> Vec<Vec<i64>> {
        v.sort_by_key(|a| format!("{a:?}"));
        v
    }

    fn test_connect(input: Vec<Vec<i64>>, expected: Vec<Vec<i64>>) {
        assert_eq!(sorted(expected), sorted(connect_polygon_segments(&input)), "connect({input:?})");
    }

    #[test]
    fn test_connect_utils() {
        let mut a = vec![1, 2];
        append_to_skip_first(&mut a, &[2, 3, 4]);
        assert_eq!(vec![1, 2, 3, 4], a);
        let mut b = vec![3, 4];
        prepend_to_skip_last(&mut b, &[1, 2, 3]);
        assert_eq!(vec![1, 2, 3, 4], b);
    }

    #[test]
    fn test_connect_polygon_segments() {
        test_connect(vec![], vec![]);
        test_connect(vec![vec![1]], vec![]);
        test_connect(vec![vec![1, 2]], vec![]);
        test_connect(vec![vec![1, 2, 1]], vec![]);
        test_connect(vec![vec![1, 2, 3, 1]], vec![vec![1, 2, 3, 1]]);
        test_connect(vec![vec![1, 2], vec![3, 2], vec![3, 1]], vec![vec![1, 2, 3, 1]]);
        test_connect(
            vec![vec![10, 11, 10], vec![1, 2], vec![3, 2], vec![3, 1], vec![4, 5], vec![5, 6], vec![6, 4]],
            vec![vec![4, 5, 6, 4], vec![1, 2, 3, 1]],
        );
        test_connect(
            vec![vec![1, 2], vec![3, 4], vec![4, 5], vec![5, 6], vec![6, 7], vec![3, 2], vec![7, 1]],
            vec![vec![1, 2, 3, 4, 5, 6, 7, 1]],
        );
    }

    #[derive(Clone, Copy)]
    struct Node {
        id: i64,
        x: f64,
        y: f64,
    }

    #[derive(Default)]
    struct Fixture {
        id: i64,
    }

    impl Fixture {
        fn node(&mut self, x: f64, y: f64) -> Node {
            self.id += 1;
            Node { id: self.id, x, y }
        }

        fn rectangle_nodes_xy(&mut self, x_min: f64, y_min: f64, x_max: f64, y_max: f64) -> Vec<Node> {
            let start_end = self.node(x_min, y_min);
            vec![start_end, self.node(x_max, y_min), self.node(x_max, y_max), self.node(x_min, y_max), start_end]
        }

        fn rectangle_nodes(&mut self, min: f64, max: f64) -> Vec<Node> {
            self.rectangle_nodes_xy(min, min, max, max)
        }
    }

    fn build_multipolygon(ways: &[Vec<Node>]) -> GeometryResult<Geometry<f64>> {
        let mut coords = HashMap::new();
        let mut rings = vec![];
        for way in ways {
            rings.push(way.iter().map(|n| n.id).collect::<Vec<_>>());
            for n in way {
                coords.insert(n.id, Coord { x: n.x, y: n.y });
            }
        }
        let lookup = |id: i64| coords.get(&id).copied();
        build(&rings, &lookup, 0)
    }

    #[track_caller]
    fn test_build_multipolygon(ways: &[Vec<Node>], expected: impl Into<Geometry<f64>>) {
        let actual = build_multipolygon(ways).unwrap();
        assert_same_normalized_feature(&expected.into(), &actual);
    }

    #[test]
    fn test_connect_simple_polygon() {
        let mut f = Fixture::default();
        let node1 = f.node(0.5, 0.5);
        let node2 = f.node(0.75, 0.5);
        let node3 = f.node(0.75, 0.75);
        test_build_multipolygon(
            &[vec![node1, node2], vec![node2, node3], vec![node1, node3]],
            new_polygon(&[0.5, 0.5, 0.75, 0.5, 0.75, 0.75, 0.5, 0.5]),
        );
    }

    #[test]
    fn test_connect_almost_closed() {
        let mut f = Fixture::default();
        let node1 = f.node(0.5, 0.5);
        let node1a = f.node(0.5 + 1e-10, 0.5);
        let node2 = f.node(0.75, 0.5);
        let node3 = f.node(0.75, 0.75);
        test_build_multipolygon(
            &[vec![node1, node2], vec![node2, node3], vec![node1a, node3]],
            new_polygon(&[0.5, 0.5, 0.75, 0.5, 0.75, 0.75, 0.5, 0.5]),
        );
    }

    #[test]
    fn test_build_multipolygon_from_geometries() {
        let actual = build_from_coordinates(&[
            new_coordinate_list(&[0.2, 0.2, 0.4, 0.2, 0.4, 0.4]),
            new_coordinate_list(&[0.4, 0.4, 0.2, 0.4, 0.2, 0.2]),
        ])
        .unwrap();
        assert_same_normalized_feature(&rectangle(0.2, 0.4).into(), &actual);
    }

    #[test]
    fn test_throw_when_no_closed() {
        let mut f = Fixture::default();
        let node1 = f.node(0.5, 0.5);
        let node1a = f.node(0.5 + 1e-1, 0.5);
        let node2 = f.node(0.75, 0.5);
        let node3 = f.node(0.75, 0.75);
        let err = build_multipolygon(&[vec![node1, node2], vec![node2, node3], vec![node1a, node3]]).unwrap_err();
        assert_eq!("osm_invalid_multipolygon_empty_after_fix", err.stat());
    }

    #[test]
    fn test_throw_when_empty() {
        let err = build_multipolygon(&[]).unwrap_err();
        assert_eq!("osm_invalid_multipolygon_empty", err.stat());
    }

    #[test]
    fn test_throw_when_missing_node() {
        let rings = vec![vec![1, 2, 3, 1]];
        let lookup = |id: i64| (id != 2).then_some(Coord { x: id as f64, y: (id * id) as f64 });
        let err = build(&rings, &lookup, 5).unwrap_err();
        assert_eq!("osm_invalid_multipolygon", err.stat());
        assert!(err.message().contains("Missing location for node: 2"), "{}", err.message());
    }

    #[test]
    fn test_long_long_map_provider() {
        let map = crate::collection::long_long_map::new_in_memory_sorted_table();
        let encode = |lon, lat| geo_utils::encode_flat_location(lon, lat) as i64;
        {
            let mut writer = map.new_writer();
            writer.put(1, encode(0.0, 0.0)).unwrap();
            writer.put(2, encode(1.0, 0.0)).unwrap();
            writer.put(3, encode(1.0, 1.0)).unwrap();
        }
        let provider = LongLongMapLocationProvider(map.as_ref());
        let geom = build(&[vec![1, 2, 3, 1]], &provider, 1).unwrap();
        assert!(matches!(geom, Geometry::Polygon(_)));
        assert!(provider.get_coordinate(4).is_err());
    }

    #[test]
    fn test_ignore_single_not_closed() {
        let mut f = Fixture::default();
        let rect = f.rectangle_nodes(0.0, 10.0);
        let open = vec![f.node(20.0, 20.0), f.node(30.0, 20.0), f.node(30.0, 30.0), f.node(20.0, 30.0)];
        test_build_multipolygon(&[rect, open], rectangle(0.0, 10.0));
    }

    #[test]
    fn test_simple_polygon_with_hole() {
        let mut f = Fixture::default();
        let ways = [f.rectangle_nodes(0.0, 10.0), f.rectangle_nodes(2.0, 8.0)];
        test_build_multipolygon(
            &ways,
            new_polygon_with_holes(rectangle_coord_list(0.0, 10.0), vec![rectangle_coord_list(2.0, 8.0)]),
        );
    }

    #[test]
    fn test_simple_polygon_with_multiple_holes() {
        let mut f = Fixture::default();
        let ways = [f.rectangle_nodes(0.0, 10.0), f.rectangle_nodes(1.0, 2.0), f.rectangle_nodes(3.0, 4.0)];
        test_build_multipolygon(
            &ways,
            new_polygon_with_holes(
                rectangle_coord_list(0.0, 10.0),
                vec![rectangle_coord_list(1.0, 2.0), rectangle_coord_list(3.0, 4.0)],
            ),
        );
    }

    #[test]
    fn test_multi_polygon_with_nested_holes() {
        let mut f = Fixture::default();
        let ways = [
            f.rectangle_nodes(0.0, 10.0),
            f.rectangle_nodes(1.0, 9.0),
            f.rectangle_nodes(2.0, 8.0),
            f.rectangle_nodes(3.0, 7.0),
            f.rectangle_nodes(4.0, 6.0),
        ];
        test_build_multipolygon(
            &ways,
            new_multi_polygon(vec![
                new_polygon_with_holes(rectangle_coord_list(0.0, 10.0), vec![rectangle_coord_list(1.0, 9.0)]),
                new_polygon_with_holes(rectangle_coord_list(2.0, 8.0), vec![rectangle_coord_list(3.0, 7.0)]),
                rectangle(4.0, 6.0),
            ]),
        );
    }

    #[test]
    fn test_touching_polygons_with_hole() {
        let mut f = Fixture::default();
        let ways =
            [f.rectangle_nodes(0.0, 10.0), f.rectangle_nodes_xy(10.0, 0.0, 30.0, 10.0), f.rectangle_nodes(2.0, 8.0)];
        test_build_multipolygon(
            &ways,
            new_multi_polygon(vec![
                new_polygon_with_holes(rectangle_coord_list(0.0, 10.0), vec![rectangle_coord_list(2.0, 8.0)]),
                rectangle_xy(10.0, 0.0, 30.0, 10.0),
            ]),
        );
    }

    #[test]
    fn test_broken_polygon_self_intersect1() {
        let mut f = Fixture::default();
        let start_end = f.node(0.0, 0.0);
        let outer = vec![
            start_end,
            f.node(0.0, 10.0),
            f.node(10.0, 10.0),
            f.node(10.0, 0.0),
            f.node(20.0, 0.0),
            f.node(20.0, 10.0),
            f.node(30.0, 10.0),
            f.node(30.0, 0.0),
            start_end,
        ];
        let inner = f.rectangle_nodes(2.0, 8.0);
        test_build_multipolygon(
            &[outer, inner],
            new_polygon_with_holes(
                new_coordinate_list(&[
                    0.0, 0.0, 0.0, 10.0, 10.0, 10.0, 10.0, 0.0, 20.0, 0.0, 20.0, 10.0, 30.0, 10.0, 30.0, 0.0, 0.0, 0.0,
                ]),
                vec![rectangle_coord_list(2.0, 8.0)],
            ),
        );
    }

    #[test]
    fn test_broken_polygon_self_intersect2() {
        let mut f = Fixture::default();
        let start_end = f.node(10.0, 0.0);
        let outer = vec![
            start_end,
            f.node(10.0, 0.0),
            f.node(0.0, 0.0),
            f.node(0.0, 10.0),
            f.node(10.0, 10.0),
            f.node(10.0, 0.0),
            f.node(20.0, 0.0),
            f.node(20.0, 10.0),
            f.node(30.0, 10.0),
            f.node(30.0, 0.0),
            start_end,
        ];
        let inner = f.rectangle_nodes(2.0, 8.0);
        test_build_multipolygon(
            &[outer, inner],
            new_polygon_with_holes(
                new_coordinate_list(&[
                    10.0, 0.0, 10.0, 0.0, 0.0, 0.0, 0.0, 10.0, 10.0, 10.0, 10.0, 0.0, 20.0, 0.0, 20.0, 10.0, 30.0,
                    10.0, 30.0, 0.0, 10.0, 0.0,
                ]),
                vec![rectangle_coord_list(2.0, 8.0)],
            ),
        );
    }

    fn triangle_test(delta: f64) {
        let mut f = Fixture::default();
        let start_end = f.node(0.0, 0.0);
        let outer =
            vec![start_end, f.node(0.0, 100.0), f.node(100.0, 50.0 - delta), f.node(100.0, 50.0 + delta), start_end];
        let inner = f.rectangle_nodes_xy(10.0, 45.0, 20.0, 55.0);
        test_build_multipolygon(
            &[outer, inner],
            new_polygon_with_holes(
                new_coordinate_list(&[0.0, 0.0, 0.0, 100.0, 100.0, 50.0 - delta, 100.0, 50.0 + delta, 0.0, 0.0]),
                vec![rectangle_coord_list_xy(10.0, 45.0, 20.0, 55.0)],
            ),
        );
    }

    #[test]
    fn test_broken_polygon_self_intersect_triangle_small_overlap() {
        triangle_test(0.00001);
    }

    #[test]
    fn test_broken_polygon_self_intersect_triangle_large_overlap() {
        triangle_test(1.0);
    }
}

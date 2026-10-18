//! Port of `render/TiledGeometry.java`: splits geometries represented by lists of coordinate sequences
//! ([`Groups`]) into the geometries that appear on individual tiles that the geometry touches.
//!
//! [`crate::render::geometry_coordinate_sequences`] converts between [`Geometry`] and [`Groups`] for this utility.
//!
//! Adapted from the stripe clipping algorithm in [geojson-vt](https://github.com/mapbox/geojson-vt/) (ISC License,
//! Copyright (c) 2015, Mapbox) and modified so that it eagerly produces all sliced tiles at a zoom level for each
//! input geometry.
//!
//! Differences from Java:
//! - tile contents are stored in an insertion-ordered map with a fixed-seed hasher so iteration order is
//!   deterministic; filled tiles iterate by x ascending, then y ascending.
//! - [`CoveredTiles`] stores per-column y ranges instead of a `RoaringBitmap`.
//! - per-stripe "current slice" lookups use dense vectors indexed by the column/row offset instead of hash maps, and
//!   scratch buffers are reused across stripes.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::hash::BuildHasherDefault;
use std::sync::Arc;

use geo_types::{Coord, Geometry};
use indexmap::IndexMap;

use crate::collection::int_range_set::IntRangeSet;
use crate::geo::geometry_exception::{GeometryException, GeometryResult};
use crate::geo::geometry_transformer;
use crate::geo::tile_coord::TileCoord;
use crate::geo::tile_extents::{ForZoom, TilePredicate};
use crate::render::geometry_coordinate_sequences::{self as gcs, CoordSeq, Groups};

const NEIGHBOR_BUFFER_EPS: f64 = 0.1 / 4096.0;

type FastIndexMap<K, V> = IndexMap<K, V, BuildHasherDefault<ahash::AHasher>>;

#[inline]
fn key(x: u32, y: u32) -> u64 {
    ((x as u64) << 32) | y as u64
}

#[inline]
fn unkey(k: u64) -> (u32, u32) {
    ((k >> 32) as u32, k as u32)
}

#[cfg(not(test))]
#[inline]
fn single_tile_fast_path_enabled() -> bool {
    true
}

#[cfg(test)]
thread_local! {
    /// Tests turn the single tile fast path off to compare it with the striped clipping.
    static SINGLE_TILE_FAST_PATH: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

#[cfg(test)]
fn single_tile_fast_path_enabled() -> bool {
    SINGLE_TILE_FAST_PATH.with(|f| f.get())
}

#[inline]
fn floor_i32(v: f64) -> i32 {
    v.floor() as i32
}

/// Adds `(x, y)` to `seq` unless it equals the current endpoint (Java `MutableCoordinateSequence.addPoint`).
#[inline]
fn add_point(seq: &mut CoordSeq, x: f64, y: f64) {
    match seq.last() {
        Some(last) if last.x == x && last.y == y => {}
        _ => seq.push(Coord { x, y }),
    }
}

/// Adds `(x, y)` scaled into tile pixel coordinates of row `rel_y` (Java's `newScalingSequence(0, y, 256)`).
#[inline]
fn add_scaled(seq: &mut CoordSeq, x: f64, y: f64, rel_y: i32) {
    add_point(seq, 256.0 * x, 256.0 * (y - rel_y as f64));
}

#[inline]
fn close_ring(seq: &mut CoordSeq) {
    if let (Some(&first), Some(last)) = (seq.first(), seq.last())
        && first != *last
    {
        seq.push(first);
    }
}

fn intersect_x(out: &mut CoordSeq, ax: f64, ay: f64, bx: f64, by: f64, x: f64) {
    let t = (x - ax) / (bx - ax);
    add_point(out, x, ay + (by - ay) * t);
}

fn intersect_y(out: &mut CoordSeq, ax: f64, ay: f64, bx: f64, by: f64, y: f64, rel_y: i32) {
    let t = (y - ay) / (by - ay);
    add_scaled(out, ax + (bx - ax) * t, y, rel_y);
}

fn fill(buffer: f64) -> CoordSeq {
    let min = -256.0 * buffer;
    let max = 256.0 - min;
    vec![
        Coord { x: min, y: min },
        Coord { x: max, y: min },
        Coord { x: max, y: max },
        Coord { x: min, y: max },
        Coord { x: min, y: min },
    ]
}

fn wrap_int(value: i32, max: i32) -> i32 {
    let v = value % max;
    if v < 0 { v + max } else { v }
}

#[derive(Clone, Copy)]
enum Side {
    Left,
    Right,
}

/// A filled range that was skipped because an edge of the polygon processed later may intersect it.
struct SkippedSegment {
    side: Side,
    lo: i32,
    hi: i32,
    asc: bool,
}

/// Overflow flags from slicing one copy of the world.
#[derive(Default, Clone, Copy)]
struct Overflow {
    left: bool,
    right: bool,
}

/// The tiles that a geometry touches at a zoom level and the contents of the geometry on each tile.
pub struct TiledGeometry {
    tile_contents: FastIndexMap<u64, Groups>,
    extents: ForZoom,
    buffer: f64,
    neighbor_buffer: f64,
    z: u32,
    area: bool,
    max_tiles_at_this_zoom: i32,
    /// Map from X coordinate to range of Y coordinates that contain filled tiles inside this geometry.
    filled_ranges: Option<BTreeMap<i32, IntRangeSet>>,
    /// Scratch: per-row index of the open slice's entry in the in-progress map for the current stripe.
    y_scratch: Vec<Option<usize>>,
    /// Scratch: per-column "slice is open" flags for the current ring.
    x_scratch: Vec<bool>,
}

impl fmt::Debug for TiledGeometry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TiledGeometry")
            .field("z", &self.z)
            .field("area", &self.area)
            .field("tiles", &self.tile_contents.len())
            .finish()
    }
}

impl TiledGeometry {
    fn new(extents: &ForZoom, buffer: f64, z: u32, area: bool) -> TiledGeometry {
        TiledGeometry {
            tile_contents: FastIndexMap::default(),
            extents: extents.clone(),
            buffer,
            // make sure we inspect neighboring tiles when a line runs along an edge
            neighbor_buffer: buffer + NEIGHBOR_BUFFER_EPS,
            z,
            area,
            max_tiles_at_this_zoom: 1i32 << z,
            filled_ranges: None,
            y_scratch: Vec::new(),
            x_scratch: Vec::new(),
        }
    }

    /// Returns all the points that appear on tiles representing points at `coords`.
    ///
    /// `buffer` is how far detail should be included beyond the edge of each tile (0=none, 1=a full tile width), and
    /// `coords` are in scaled world coordinates where `(0, 0)` is the northwest and `(2^z, 2^z)` is the southeast
    /// corner of the planet.
    pub fn slice_points_into_tiles(extents: &ForZoom, buffer: f64, z: u32, coords: &[Coord<f64>]) -> TiledGeometry {
        let mut result = TiledGeometry::new(extents, buffer, z, false);
        for coord in coords {
            result.slice_point(*coord);
        }
        result
    }

    /// Slices any point, line or polygon geometry in scaled world coordinates into tiles (Java's public
    /// `sliceIntoTiles(Geometry, ...)`).
    pub fn slice_geometry_into_tiles(
        scaled_geom: &Geometry<f64>,
        min_size: f64,
        buffer: f64,
        z: u32,
        extents: &ForZoom,
    ) -> GeometryResult<TiledGeometry> {
        if geometry_transformer::is_empty(scaled_geom) {
            return Ok(TiledGeometry::new(extents, buffer, z, false));
        }
        match scaled_geom {
            Geometry::Point(p) => Ok(Self::slice_points_into_tiles(extents, buffer, z, &[p.0])),
            Geometry::MultiPoint(mp) => {
                let coords: Vec<Coord<f64>> = mp.0.iter().map(|p| p.0).collect();
                Ok(Self::slice_points_into_tiles(extents, buffer, z, &coords))
            }
            Geometry::Polygon(_)
            | Geometry::MultiPolygon(_)
            | Geometry::Rect(_)
            | Geometry::Triangle(_)
            | Geometry::LineString(_)
            | Geometry::MultiLineString(_)
            | Geometry::Line(_) => {
                let groups = gcs::extract_groups(scaled_geom, min_size);
                let area = matches!(
                    scaled_geom,
                    Geometry::Polygon(_) | Geometry::MultiPolygon(_) | Geometry::Rect(_) | Geometry::Triangle(_)
                );
                Self::slice_into_tiles(&groups, buffer, area, z, extents)
            }
            Geometry::GeometryCollection(_) => Err(GeometryException::new(
                "unsupported_geometry_type",
                "Unsupported JTS geometry type GeometryCollection",
            )),
        }
    }

    /// Returns the set of tiles that `scaled_geom` (in scaled world coordinates where northwest is `(0, 0)` and
    /// southeast is `(2^z, 2^z)`) touches at `zoom`.
    pub fn get_covered_tiles(
        scaled_geom: &Geometry<f64>,
        zoom: u32,
        extents: &ForZoom,
    ) -> GeometryResult<CoveredTiles> {
        if geometry_transformer::is_empty(scaled_geom) {
            return Ok(CoveredTiles::empty(zoom));
        }
        match scaled_geom {
            Geometry::GeometryCollection(gc) => {
                let mut result = CoveredTiles::empty(zoom);
                for g in &gc.0 {
                    result = CoveredTiles::merge(&Self::get_covered_tiles(g, zoom, extents)?, &result);
                }
                Ok(result)
            }
            other => Ok(Self::slice_geometry_into_tiles(other, 0.0, 0.0, zoom, extents)?.covered_tiles()),
        }
    }

    /// Returns the tiles that this geometry touches, and the contents of those tiles for this geometry.
    ///
    /// `groups` are the linestrings or polygon rings extracted using
    /// [`crate::render::geometry_coordinate_sequences::extract_groups`] in scaled world coordinates, `area` is true
    /// for polygons.
    ///
    /// Fails for a polygon that is invalid in a way that interferes with clipping.
    pub fn slice_into_tiles(
        groups: &Groups,
        buffer: f64,
        area: bool,
        z: u32,
        extents: &ForZoom,
    ) -> GeometryResult<TiledGeometry> {
        let mut result = TiledGeometry::new(extents, buffer, z, area);
        let wrap_result = result.slice_world_copy(groups, 0)?;
        if wrap_result.right {
            result.slice_world_copy(groups, -result.max_tiles_at_this_zoom)?;
        }
        if wrap_result.left {
            result.slice_world_copy(groups, result.max_tiles_at_this_zoom)?;
        }
        // free scratch space
        result.x_scratch = Vec::new();
        result.y_scratch = Vec::new();
        Ok(result)
    }

    fn slice_point(&mut self, coord: Coord<f64>) {
        let world_x = coord.x;
        let world_y = coord.y;
        let nb = self.neighbor_buffer;
        let min_x = floor_i32(world_x - nb);
        let max_x = floor_i32(world_x + nb);
        let min_y = (self.extents.min_y as i32).max(floor_i32(world_y - nb));
        let max_y = (self.extents.max_y as i32 - 1).min(floor_i32(world_y + nb));
        for x in min_x..=max_x {
            let tile_x = world_x - x as f64;
            let wrapped_x = wrap_int(x, self.max_tiles_at_this_zoom) as u32;
            // point may end up inside bounds after wrapping
            if self.extents.test_x(wrapped_x) {
                for y in min_y..=max_y {
                    if self.extents.test(wrapped_x, y as u32) {
                        let tile_y = world_y - y as f64;
                        let groups = self.tile_contents.entry(key(wrapped_x, y as u32)).or_insert_with(|| vec![vec![]]);
                        groups[0].push(vec![Coord { x: tile_x * 256.0, y: tile_y * 256.0 }]);
                    }
                }
            }
        }
    }

    /// The zoom level of this geometry.
    pub fn zoom_level(&self) -> u32 {
        self.z
    }

    /// Returns an iterator over the coordinates of every tile that is completely filled within this polygon at this
    /// zoom level, ordered by x ascending, y ascending.
    pub fn filled_tiles(&self) -> impl Iterator<Item = TileCoord> + '_ {
        let z = self.z;
        self.filled_ranges.iter().flat_map(|m| m.iter()).flat_map(move |(&x, ys)| {
            let x = x as u32;
            ys.iter().filter_map(move |y| {
                let y = y as u32;
                (self.extents.test(x, y) && !self.tile_contents.contains_key(&key(x, y)))
                    .then(|| TileCoord::of_xyz(x, y, z))
            })
        })
    }

    /// Returns the tiles touched by this geometry.
    pub fn covered_tiles(&self) -> CoveredTiles {
        let mut cols: BTreeMap<u32, IntRangeSet> = BTreeMap::new();
        for &k in self.tile_contents.keys() {
            let (x, y) = unkey(k);
            cols.entry(x).or_default().add(y as i32, y as i32);
        }
        if let Some(filled) = &self.filled_ranges {
            for (&x, ys) in filled {
                if !ys.is_empty() {
                    cols.entry(x as u32).or_default().add_all(ys);
                }
            }
        }
        cols.retain(|_, v| !v.is_empty());
        CoveredTiles { cols, z: self.z }
    }

    /// Returns every tile that this geometry touches and the partial geometry contained on that tile that can be
    /// reassembled using [`crate::render::geometry_coordinate_sequences`].
    pub fn tile_data(&self) -> impl Iterator<Item = (TileCoord, &Groups)> + '_ {
        let z = self.z;
        self.tile_contents.iter().map(move |(&k, v)| {
            let (x, y) = unkey(k);
            (TileCoord::of_xyz(x, y, z), v)
        })
    }

    /// The number of tiles with geometry detail (excluding filled tiles).
    pub fn num_tiles_with_data(&self) -> usize {
        self.tile_contents.len()
    }

    /// Returns the partial geometry on `tile`, if any.
    pub fn get_tile_data(&self, tile: TileCoord) -> Option<&Groups> {
        self.tile_contents.get(&key(tile.x, tile.y))
    }

    /// Like [`TiledGeometry::tile_data`] but consumes this geometry to avoid copying coordinates. Use
    /// [`TiledGeometry::filled_tiles`] first if you need them.
    pub fn into_tile_data(self) -> impl Iterator<Item = (TileCoord, Groups)> {
        let z = self.z;
        self.tile_contents.into_iter().map(move |(k, v)| {
            let (x, y) = unkey(k);
            (TileCoord::of_xyz(x, y, z), v)
        })
    }

    /// Splits this geometry into its filled tiles and the tile data so callers can consume both without copying.
    pub fn into_parts(self) -> (Vec<TileCoord>, Vec<(TileCoord, Groups)>) {
        let filled = self.filled_tiles().collect();
        (filled, self.into_tile_data().collect())
    }

    /// Slices a geometry into tiles and stores in member fields for a single "copy" of the world.
    ///
    /// Instead of handling content outside -180 to 180 degrees longitude, returns whether this method should be called
    /// again with a different `x_offset` to process wrapped content.
    fn slice_world_copy(&mut self, groups: &Groups, x_offset: i32) -> GeometryResult<Overflow> {
        let mut overflow = Overflow::default();
        let mut in_progress: FastIndexMap<u64, Vec<CoordSeq>> = FastIndexMap::default();
        for group in groups {
            if x_offset == 0 && single_tile_fast_path_enabled() && self.slice_single_tile_group(group) {
                continue;
            }
            in_progress.clear();
            for (i, segment) in group.iter().enumerate() {
                let is_outer_ring = i == 0;
                // Step 1 in the striped clipping algorithm: slice the geometry into vertical slices representing
                // each "x" tile coordinate.
                let x_slices = self.slice_x(segment);
                for (cx, stripes) in x_slices {
                    let x = cx + x_offset;
                    // skip processing content past the edge of the world, but return that we saw it
                    if x >= self.max_tiles_at_this_zoom {
                        overflow.right = true;
                    } else if x < 0 {
                        overflow.left = true;
                    } else {
                        // Step 2 in the striped clipping algorithm: split each vertical column x slice into horizontal
                        // slices representing the row for each Y coordinate.
                        for stripe in &stripes {
                            // slice_y only stores content for rings of a polygon, need to store the filled tiles that it
                            // spanned separately
                            let filled_y_range = self.slice_y(stripe, x, is_outer_ring, &mut in_progress)?;
                            if self.area
                                && let Some(range) = filled_y_range
                            {
                                if is_outer_ring {
                                    self.add_filled_range(x, range);
                                } else {
                                    self.remove_filled_range(x, &range);
                                }
                            }
                        }
                    }
                }
            }
            self.add_shape_to_results(&mut in_progress);
        }
        Ok(overflow)
    }

    /// Fast path of [`Self::slice_world_copy`] for a group (a line, or the rings of one polygon) whose coordinates,
    /// expanded by the neighbor buffer, all fall in one tile: no clipping happens, so every ring becomes the same
    /// sequence of points (consecutive duplicates removed, polygon rings closed) translated and scaled to that tile,
    /// exactly what the striped clipping produces. Returns false (doing nothing) if the group does not qualify.
    fn slice_single_tile_group(&mut self, group: &[CoordSeq]) -> bool {
        let nb = self.neighbor_buffer;
        let (mut min_x, mut min_y, mut max_x, mut max_y) =
            (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        if group.is_empty() {
            return false;
        }
        for ring in group {
            if ring.len() < 2 {
                return false;
            }
            for c in ring {
                if !(c.x.is_finite() && c.y.is_finite()) {
                    return false;
                }
                min_x = min_x.min(c.x);
                max_x = max_x.max(c.x);
                min_y = min_y.min(c.y);
                max_y = max_y.max(c.y);
            }
        }
        let x = floor_i32(min_x - nb);
        let y = floor_i32(min_y - nb);
        if floor_i32(max_x + nb) != x
            || floor_i32(max_y + nb) != y
            || x < 0
            || x >= self.max_tiles_at_this_zoom
            || !self.extents.test_x(x as u32)
            || y < self.extents.min_y as i32
            || y >= self.extents.max_y as i32
        {
            return false;
        }
        let area = self.area;
        let (fx, fy) = (x as f64, y as f64);
        let mut seqs: Vec<CoordSeq> = Vec::with_capacity(group.len());
        for ring in group {
            // the x stripe (tile-relative x, world y), then the row (tile pixels), as in slice_x / slice_y
            let mut stripe: CoordSeq = Vec::with_capacity(ring.len() + 1);
            for c in ring {
                add_point(&mut stripe, c.x - fx, c.y);
            }
            if area {
                close_ring(&mut stripe);
            }
            let mut out: CoordSeq = Vec::with_capacity(stripe.len() + 1);
            for c in &stripe {
                add_point(&mut out, 256.0 * c.x, 256.0 * (c.y - fy));
            }
            if area {
                close_ring(&mut out);
            }
            seqs.push(out);
        }
        // add_shape_to_results
        let min_points = if area { 4 } else { 2 };
        if area && seqs[0].len() < 4 {
            return true;
        }
        if !self.extents.test(x as u32, y as u32) {
            return true;
        }
        seqs.retain(|seq| seq.len() >= min_points);
        if !seqs.is_empty() {
            self.tile_contents.entry(key(x as u32, y as u32)).or_default().push(seqs);
        }
        true
    }

    fn add_shape_to_results(&mut self, in_progress: &mut FastIndexMap<u64, Vec<CoordSeq>>) {
        let area = self.area;
        let min_points = if area { 4 } else { 2 };
        for (tile_id, in_seqs) in in_progress.drain(..) {
            if area && in_seqs[0].len() < 4 {
                // not enough points in outer polygon, ignore
                continue;
            }
            let (x, y) = unkey(tile_id);
            if !self.extents.test(x, y) {
                continue;
            }
            let out_seqs: Vec<CoordSeq> = in_seqs.into_iter().filter(|seq| seq.len() >= min_points).collect();
            if !out_seqs.is_empty() {
                self.tile_contents.entry(tile_id).or_default().push(out_seqs);
            }
        }
    }

    /// Returns the segments of this geometry that cross each vertical column formed by all tiles where `x=x`, as
    /// `(x, segments)` pairs.
    fn slice_x(&mut self, segment: &[Coord<f64>]) -> Vec<(i32, Vec<CoordSeq>)> {
        if segment.is_empty() {
            return Vec::new();
        }
        let left_limit = -self.buffer;
        let right_limit = 1.0 + self.buffer;
        let nb = self.neighbor_buffer;
        let area = self.area;

        let (mut min_all, mut max_all) = (f64::INFINITY, f64::NEG_INFINITY);
        for c in segment {
            min_all = min_all.min(c.x);
            max_all = max_all.max(c.x);
        }
        let x_lo = floor_i32(min_all - nb);
        let x_hi = floor_i32(max_all + nb);
        let width = (x_hi as i64 - x_lo as i64 + 1).max(0) as usize;
        let mut new_geoms: Vec<Vec<CoordSeq>> = Vec::with_capacity(width);
        new_geoms.resize_with(width, Vec::new);
        let open = &mut self.x_scratch;
        open.clear();
        open.resize(width, false);

        for w in segment.windows(2) {
            let (ax, ay, bx, by) = (w[0].x, w[0].y, w[1].x, w[1].y);
            let min_x = ax.min(bx);
            let max_x = ax.max(bx);
            let start_x = floor_i32(min_x - nb);
            let end_x = floor_i32(max_x + nb);

            // for each column this segment crosses
            for x in start_x..=end_x {
                let idx = (x - x_lo) as usize;
                let ax_tile = ax - x as f64;
                let bx_tile = bx - x as f64;
                let col = &mut new_geoms[idx];
                if !open[idx] {
                    open[idx] = true;
                    col.push(Vec::new());
                }
                let slice = col.last_mut().unwrap();

                let mut exited = false;

                if ax_tile < left_limit {
                    // ---|-->  | (line enters the clip region from the left)
                    if bx_tile > left_limit {
                        intersect_x(slice, ax_tile, ay, bx_tile, by, left_limit);
                    }
                } else if ax_tile > right_limit {
                    // |  <--|--- (line enters the clip region from the right)
                    if bx_tile < right_limit {
                        intersect_x(slice, ax_tile, ay, bx_tile, by, right_limit);
                    }
                } else {
                    // | --> | (line starts inside)
                    add_point(slice, ax_tile, ay);
                }
                if bx_tile < left_limit && ax_tile >= left_limit {
                    // <--|---  | or <--|-----|--- (line exits the clip region on the left)
                    intersect_x(slice, ax_tile, ay, bx_tile, by, left_limit);
                    exited = true;
                }
                if bx_tile > right_limit && ax_tile <= right_limit {
                    // |  ---|--> or ---|-----|--> (line exits the clip region on the right)
                    intersect_x(slice, ax_tile, ay, bx_tile, by, right_limit);
                    exited = true;
                }

                if !area && exited {
                    open[idx] = false;
                }
            }
        }
        // add the last point
        let last = segment[segment.len() - 1];
        let (ax, ay) = (last.x, last.y);
        let start_x = floor_i32(ax - nb);
        let end_x = floor_i32(ax + nb);
        for x in (start_x - 1)..=(end_x + 1) {
            if x < x_lo || x > x_hi {
                continue;
            }
            let idx = (x - x_lo) as usize;
            let ax_tile = ax - x as f64;
            if open[idx] && ax_tile >= left_limit && ax_tile <= right_limit {
                add_point(new_geoms[idx].last_mut().unwrap(), ax_tile, ay);
            }
        }

        // close the polygons if endpoints are not the same after clipping
        if area {
            for (idx, col) in new_geoms.iter_mut().enumerate() {
                if open[idx]
                    && let Some(slice) = col.last_mut()
                {
                    close_ring(slice);
                }
            }
        }
        let max = self.max_tiles_at_this_zoom;
        let extents = &self.extents;
        new_geoms
            .into_iter()
            .enumerate()
            .filter(|(_, col)| !col.is_empty())
            .map(|(idx, col)| (idx as i32 + x_lo, col))
            .filter(|(x, _)| extents.test_x(wrap_int(*x, max) as u32))
            .collect()
    }

    /// Splits an entire vertical X column of edge segments into Y rows that form `(x, y)` tile coordinates at this
    /// zoom level, stores the result in `in_progress` and returns the Y ranges of filled tile coordinates if this is a
    /// polygon.
    fn slice_y(
        &mut self,
        stripe: &[Coord<f64>],
        x: i32,
        outer: bool,
        in_progress: &mut FastIndexMap<u64, Vec<CoordSeq>>,
    ) -> GeometryResult<Option<IntRangeSet>> {
        if stripe.is_empty() {
            return Ok(None);
        }
        if x < 0 || x >= self.max_tiles_at_this_zoom {
            return Ok(None);
        }
        let buffer = self.buffer;
        let nb = self.neighbor_buffer;
        let area = self.area;
        let left_edge = -buffer;
        let right_edge = 1.0 + buffer;
        let extent_min_y = self.extents.min_y as i32;
        let extent_max_y = self.extents.max_y as i32;

        let mut tile_ys_with_detail: Option<BTreeSet<i32>> = None;
        let mut right_filled: Option<IntRangeSet> = None;
        let mut left_filled: Option<IntRangeSet> = None;
        // keep a record of filled tiles that we skipped because an edge of the polygon that gets processed
        // later may intersect the edge of a filled tile, and we'll need to replay all the edges we skipped
        let mut skipped: Option<Vec<SkippedSegment>> = None;

        // dense map from row (offset by y_lo) to the index of the open slice's tile in `in_progress`
        let (mut min_all, mut max_all) = (f64::INFINITY, f64::NEG_INFINITY);
        for c in stripe {
            min_all = min_all.min(c.y);
            max_all = max_all.max(c.y);
        }
        let y_lo = extent_min_y.max(floor_i32(min_all - nb));
        let y_hi = (extent_max_y - 1).min(floor_i32(max_all + nb));
        let height = (y_hi as i64 - y_lo as i64 + 1).max(0) as usize;
        let mut y_slices = std::mem::take(&mut self.y_scratch);
        y_slices.clear();
        y_slices.resize(height, None);

        let ux = x as u32;
        let result = (|| {
            for w in stripe.windows(2) {
                let (ax, ay, bx, by) = (w[0].x, w[0].y, w[1].x, w[1].y);
                let min_y = ay.min(by);
                let max_y = ay.max(by);

                let start_y = extent_min_y.max(floor_i32(min_y - nb));
                let end_start_y = extent_min_y.max(floor_i32(min_y + nb));
                let start_end_y = (extent_max_y - 1).min(floor_i32(max_y - nb));
                let end_y = (extent_max_y - 1).min(floor_i32(max_y + nb));

                // inside a fill if one edge of the polygon runs straight down the right side or up the left side of
                // the column
                let on_right_edge = area && ax == bx && ax == right_edge;
                let on_left_edge = area && ax == bx && ax == left_edge;

                let mut y = start_y;
                while y <= end_y {
                    // skip over filled tiles until we get to the next tile that already has detail on it
                    if area && y > end_start_y && y < start_end_y && (on_right_edge || on_left_edge) {
                        let ys = tile_ys_with_detail.get_or_insert_with(|| {
                            y_slices
                                .iter()
                                .enumerate()
                                .filter(|(_, s)| s.is_some())
                                .map(|(i, _)| i as i32 + y_lo)
                                .collect()
                        });
                        let next = ys.range(y..).next().copied();
                        let next_non_edge_tile = next.map_or(start_end_y, |n| n.min(start_end_y));
                        let end_skip = next_non_edge_tile - 1;
                        // save the Y range that we skipped in case a later edge intersects a filled tile
                        if end_skip >= y {
                            skipped.get_or_insert_with(Vec::new).push(SkippedSegment {
                                side: if on_left_edge { Side::Left } else { Side::Right },
                                lo: y,
                                hi: end_skip,
                                asc: by > ay,
                            });
                            if right_filled.is_none() {
                                right_filled = Some(IntRangeSet::new());
                                left_filled = Some(IntRangeSet::new());
                            }
                            // A tile is inside a filled region when there is an odd number of vertical edges to the
                            // left and right, so keep track of this number by xor'ing the left and right fills
                            // repeatedly, then and'ing them together at the end.
                            if on_right_edge {
                                right_filled.as_mut().unwrap().xor(y, end_skip);
                            } else {
                                left_filled.as_mut().unwrap().xor(y, end_skip);
                            }
                            y = next_non_edge_tile;
                        }
                    }

                    // emit linestring/polygon ring detail
                    let top_limit = y as f64 - buffer;
                    let bottom_limit = y as f64 + 1.0 + buffer;
                    let row = (y - y_lo) as usize;
                    let entry_idx = match y_slices[row] {
                        Some(idx) => idx,
                        None => {
                            if let Some(ys) = tile_ys_with_detail.as_mut() {
                                ys.insert(y);
                            }
                            let entry = in_progress.entry(key(ux, y as u32));
                            let idx = entry.index();
                            let to_add_to = entry.or_default();
                            // if this is tile is inside a fill from an outer tile, infer that fill here
                            if area && !outer && to_add_to.is_empty() {
                                // since we process outer shells before holes, if a hole is the first thing to intersect
                                // a tile then it must be inside a filled tile from the outer shell. If that's not the
                                // case then the geometry is invalid, so fail so the caller can decide how to handle,
                                // for example fix the polygon then try again.
                                if !is_filled(&self.filled_ranges, x, y) {
                                    return Err(GeometryException::new(
                                        "bad_polygon_fill",
                                        format!("{x}, {y} is not filled!"),
                                    ));
                                }
                                to_add_to.push(fill(buffer));
                            }
                            let mut slice: CoordSeq = Vec::new();
                            // if this tile was skipped because we skipped an edge, and now it needs more points,
                            // backfill all the edges that we skipped for it
                            if area
                                && let (Some(lf), Some(rf), Some(skipped)) =
                                    (left_filled.as_ref(), right_filled.as_ref(), skipped.as_ref())
                                && (lf.contains(y) || rf.contains(y))
                            {
                                for seg in skipped {
                                    if seg.lo <= y && seg.hi >= y {
                                        let top = y as f64 - buffer;
                                        let bottom = y as f64 + 1.0 + buffer;
                                        let (start, end) = if seg.asc { (top, bottom) } else { (bottom, top) };
                                        let edge_x = match seg.side {
                                            Side::Left => -buffer,
                                            Side::Right => 1.0 + buffer,
                                        };
                                        add_scaled(&mut slice, edge_x, start, y);
                                        add_scaled(&mut slice, edge_x, end, y);
                                    }
                                }
                            }
                            to_add_to.push(slice);
                            y_slices[row] = Some(idx);
                            idx
                        }
                    };
                    let slice = in_progress[entry_idx].last_mut().unwrap();

                    let mut exited = false;

                    if ay < top_limit {
                        // ---|-->  | (line enters the clip region from the top)
                        if by > top_limit {
                            intersect_y(slice, ax, ay, bx, by, top_limit, y);
                        }
                    } else if ay > bottom_limit {
                        // |  <--|--- (line enters the clip region from the bottom)
                        if by < bottom_limit {
                            intersect_y(slice, ax, ay, bx, by, bottom_limit, y);
                        }
                    } else {
                        // | --> | (line starts inside the clip region)
                        add_scaled(slice, ax, ay, y);
                    }
                    if by < top_limit && ay >= top_limit {
                        // <--|---  | or <--|-----|--- (line exits the clip region on the top)
                        intersect_y(slice, ax, ay, bx, by, top_limit, y);
                        exited = true;
                    }
                    if by > bottom_limit && ay <= bottom_limit {
                        // |  ---|--> or ---|-----|--> (line exits the clip region on the bottom)
                        intersect_y(slice, ax, ay, bx, by, bottom_limit, y);
                        exited = true;
                    }

                    if !area && exited {
                        y_slices[row] = None;
                    }
                    y += 1;
                }
            }

            // add the last point
            let last = stripe[stripe.len() - 1];
            let (ax, ay) = (last.x, last.y);
            let start_y = floor_i32(ay - nb);
            let end_y = floor_i32(ay + nb);
            for y in (start_y - 1)..=(end_y + 1) {
                if y < y_lo || y > y_hi {
                    continue;
                }
                let k1 = y as f64 - buffer;
                let k2 = y as f64 + 1.0 + buffer;
                if ay >= k1
                    && ay <= k2
                    && let Some(idx) = y_slices[(y - y_lo) as usize]
                {
                    add_scaled(in_progress[idx].last_mut().unwrap(), ax, ay, y);
                }
            }
            // close the polygons if endpoints are not the same after clipping
            if area {
                for idx in y_slices.iter().flatten() {
                    close_ring(in_progress[*idx].last_mut().unwrap());
                }
            }
            // a tile is filled if we skipped over the entire left and right side of it while processing a polygon
            Ok(match (right_filled, left_filled) {
                (Some(mut r), Some(l)) => {
                    r.intersect(&l);
                    Some(r)
                }
                _ => None,
            })
        })();
        self.y_scratch = y_slices;
        result
    }

    fn add_filled_range(&mut self, x: i32, y_range: IntRangeSet) {
        if y_range.is_empty() {
            return;
        }
        let filled = self.filled_ranges.get_or_insert_with(BTreeMap::new);
        match filled.get_mut(&x) {
            Some(existing) => {
                existing.add_all(&y_range);
            }
            None => {
                filled.insert(x, y_range);
            }
        }
    }

    fn remove_filled_range(&mut self, x: i32, y_range: &IntRangeSet) {
        if y_range.is_empty() {
            return;
        }
        let filled = self.filled_ranges.get_or_insert_with(BTreeMap::new);
        if let Some(existing) = filled.get_mut(&x) {
            existing.remove_all(y_range);
        }
    }
}

fn is_filled(filled_ranges: &Option<BTreeMap<i32, IntRangeSet>>, x: i32, y: i32) -> bool {
    filled_ranges.as_ref().and_then(|f| f.get(&x)).is_some_and(|col| col.contains(y))
}

/// A set of tiles touched by a geometry at a zoom level.
#[derive(Clone, PartialEq, Eq)]
pub struct CoveredTiles {
    /// Column x -> set of rows y.
    cols: BTreeMap<u32, IntRangeSet>,
    z: u32,
}

impl CoveredTiles {
    fn empty(z: u32) -> CoveredTiles {
        CoveredTiles { cols: BTreeMap::new(), z }
    }

    /// Returns the union of tiles covered by `a` and `b`.
    ///
    /// # Panics
    ///
    /// If `a` and `b` have different zoom levels.
    pub fn merge(a: &CoveredTiles, b: &CoveredTiles) -> CoveredTiles {
        assert_eq!(a.z, b.z, "Cannot combine CoveredTiles with different zoom levels");
        let mut result = a.clone();
        for (x, ys) in &b.cols {
            result.cols.entry(*x).or_default().add_all(ys);
        }
        result
    }

    /// The zoom level of these tiles.
    pub fn z(&self) -> u32 {
        self.z
    }

    /// Returns true if tile `(x, y)` is covered.
    pub fn test(&self, x: u32, y: u32) -> bool {
        y <= i32::MAX as u32 && self.cols.get(&x).is_some_and(|ys| ys.contains(y as i32))
    }

    /// The number of covered tiles.
    pub fn cardinality(&self) -> u64 {
        self.cols.values().map(IntRangeSet::cardinality).sum()
    }

    /// Iterates over covered tiles ordered by x ascending, then y ascending.
    pub fn iter(&self) -> impl Iterator<Item = TileCoord> + '_ {
        let z = self.z;
        self.cols.iter().flat_map(move |(&x, ys)| ys.iter().map(move |y| TileCoord::of_xyz(x, y as u32, z)))
    }

    /// Converts this into a [`TilePredicate`] for [`ForZoom::with_shape`].
    pub fn into_predicate(self) -> TilePredicate {
        Arc::new(move |x, y| self.test(x, y))
    }
}

impl fmt::Debug for CoveredTiles {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for CoveredTiles {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CoveredTiles{{z={}, tiles={}}}", self.z, self.cardinality())
    }
}

impl<'a> IntoIterator for &'a CoveredTiles {
    type Item = TileCoord;
    type IntoIter = Box<dyn Iterator<Item = TileCoord> + 'a>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

/// Encodes tile `(x, y)` into a single int (Java's `TiledGeometry.encode`).
pub fn encode(max_tiles_at_this_zoom: u64, x: u64, y: u64) -> u32 {
    (max_tiles_at_this_zoom.wrapping_mul(x).wrapping_add(y)) as u32
}

/// Decodes a tile encoded with [`encode`] (Java's `TiledGeometry.decode`).
pub fn decode(max_tiles_at_zoom: u64, tile_id: u64, z: u32) -> TileCoord {
    TileCoord::of_xyz((tile_id / max_tiles_at_zoom) as u32, (tile_id % max_tiles_at_zoom) as u32, z)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::geo::test_utils::*;

    const Z14_TILES: u32 = 1 << 14;
    const Z16_TILES: u32 = 1 << 16;

    fn for_zoom(z: u32, min_x: u32, min_y: u32, max_x: u32, max_y: u32) -> ForZoom {
        ForZoom { z, min_x, min_y, max_x, max_y, shape_filter: None }
    }

    fn tc(x: u32, y: u32, z: u32) -> (u32, u32, u32) {
        let t = TileCoord::of_xyz(x, y, z);
        (t.x, t.y, t.z)
    }

    fn set(tiles: &CoveredTiles) -> HashSet<(u32, u32, u32)> {
        tiles.iter().map(|t| (t.x, t.y, t.z)).collect()
    }

    fn gc(geoms: Vec<Geometry<f64>>) -> Geometry<f64> {
        Geometry::GeometryCollection(new_geometry_collection(geoms))
    }

    /// The single tile fast path gives exactly the striped clipping's result (tile order, sequences, filled tiles).
    #[test]
    fn single_tile_fast_path_matches_striped_clipping() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut rand = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f64 / (1u64 << 53) as f64
        };
        let z = 3;
        let extents = for_zoom(z, 1, 0, 8, 7);
        let mut single_tile = 0;
        for case in 0..20_000 {
            let area = case % 2 == 0;
            let buffer = [0.0, 4.0 / 256.0, 0.125][case % 3];
            // groups of small rings, sometimes near tile edges or outside the extents
            let mut groups: Groups = Vec::new();
            for _ in 0..(1 + case % 3) {
                let (cx, cy) = (rand() * 9.0 - 0.5, rand() * 9.0 - 0.5);
                let size = [0.001, 0.05, 0.3][case % 5 % 3];
                let mut group = Vec::new();
                for r in 0..(1 + case % 2) {
                    let s = size / (1 + r) as f64;
                    let n = 2 + (rand() * 6.0) as usize;
                    let mut ring: CoordSeq = (0..n)
                        .map(|i| {
                            let a = i as f64 / n as f64 * std::f64::consts::TAU;
                            Coord { x: cx + s * a.cos() * rand(), y: cy + s * a.sin() * rand() }
                        })
                        .collect();
                    if case % 7 == 0 {
                        ring.push(ring[ring.len() - 1]);
                    }
                    if area {
                        ring.push(ring[0]);
                    }
                    group.push(ring);
                }
                groups.push(group);
            }
            let run = |fast: bool| {
                SINGLE_TILE_FAST_PATH.with(|f| f.set(fast));
                let result = TiledGeometry::slice_into_tiles(&groups, buffer, area, z, &extents);
                SINGLE_TILE_FAST_PATH.with(|f| f.set(true));
                result.map(|t| {
                    let filled: Vec<_> = t.filled_tiles().collect();
                    let data: Vec<_> = t.tile_data().map(|(c, g)| (c, g.clone())).collect();
                    (filled, data)
                })
            };
            let (general, fast) = (run(false), run(true));
            match (general, fast) {
                (Ok(general), Ok(fast)) => {
                    single_tile += (general.1.len() == 1) as usize;
                    assert_eq!(general, fast, "case {case}: {groups:?}");
                }
                (Err(_), Err(_)) => {}
                (general, fast) => panic!("case {case}: {general:?} vs {fast:?}"),
            }
        }
        assert!(single_tile > 2000, "{single_tile}");
    }

    fn covered(g: impl Into<Geometry<f64>>, z: u32) -> CoveredTiles {
        let max = 1 << z;
        TiledGeometry::get_covered_tiles(&g.into(), z, &for_zoom(z, 0, 0, max, max)).unwrap()
    }

    #[test]
    fn test_point_zoom16() {
        let tiled = covered(new_point(0.5, 0.5), 16);
        assert!(tiled.test(0, 0));
        assert!(!tiled.test(0, 1));
        assert!(!tiled.test(1, 0));
        assert_eq!(HashSet::from([tc(0, 0, 16)]), set(&tiled));

        let tiled = covered(new_point(Z16_TILES as f64 - 0.5, Z16_TILES as f64 - 0.5), 16);
        assert!(tiled.test(Z16_TILES - 1, Z16_TILES - 1));
        assert!(!tiled.test(Z16_TILES - 2, Z16_TILES - 1));
        assert!(!tiled.test(Z16_TILES - 1, Z16_TILES - 2));
        assert_eq!(HashSet::from([tc(Z16_TILES - 1, Z16_TILES - 1, 16)]), set(&tiled));
    }

    #[test]
    fn test_line_zoom16() {
        let tiled = covered(new_line_string(&[0.5, 0.5, 1.5, 0.5]), 16);
        assert_eq!(HashSet::from([tc(0, 0, 16), tc(1, 0, 16)]), set(&tiled));
    }

    #[test]
    fn test_polygon_zoom16() {
        let tiled = covered(rectangle(0.1, 1.9), 16);
        assert_eq!(HashSet::from([tc(0, 0, 16), tc(0, 1, 16), tc(1, 0, 16), tc(1, 1, 16)]), set(&tiled));
    }

    #[test]
    fn test_covered_tiles_edge_cases() {
        let tiled = covered(
            gc(vec![rectangle(0.0, 10.0).into(), rectangle((1 << 16) as f64 - 10.0, (1 << 16) as f64).into()]),
            16,
        );
        let (mut top_left, mut bottom_left, mut top_right, mut bottom_right) = (false, false, false, false);
        for coord in &tiled {
            if coord.x == 0 {
                if coord.y == 0 {
                    top_left = true;
                } else if coord.y == 65535 {
                    bottom_left = true;
                }
            } else if coord.x == 65535 {
                if coord.y == 0 {
                    top_right = true;
                } else if coord.y == 65535 {
                    bottom_right = true;
                }
            }
        }
        assert!(top_left);
        assert!(!top_right);
        assert!(!bottom_left);
        assert!(bottom_right);
    }

    #[test]
    fn test_point() {
        let tiled = covered(new_point(0.5, 0.5), 14);
        assert!(tiled.test(0, 0));
        assert!(!tiled.test(0, 1));
        assert!(!tiled.test(1, 0));
        assert!(!tiled.test(1, 1));
        assert_eq!(HashSet::from([tc(0, 0, 14)]), set(&tiled));

        let tiled = covered(new_point(Z14_TILES as f64 - 0.5, Z14_TILES as f64 - 0.5), 14);
        assert!(tiled.test(Z14_TILES - 1, Z14_TILES - 1));
        assert!(!tiled.test(Z14_TILES - 2, Z14_TILES - 1));
        assert!(!tiled.test(Z14_TILES - 1, Z14_TILES - 2));
        assert!(!tiled.test(Z14_TILES - 2, Z14_TILES - 2));
        assert_eq!(HashSet::from([tc(Z14_TILES - 1, Z14_TILES - 1, 14)]), set(&tiled));
    }

    #[test]
    fn test_multi_point() {
        let tiled = covered(new_multi_point(vec![new_point(0.5, 0.5), new_point(2.5, 1.5)]), 14);
        assert_eq!(HashSet::from([tc(0, 0, 14), tc(2, 1, 14)]), set(&tiled));
    }

    #[test]
    fn test_line() {
        let tiled = covered(new_line_string(&[0.5, 0.5, 1.5, 0.5]), 14);
        assert_eq!(HashSet::from([tc(0, 0, 14), tc(1, 0, 14)]), set(&tiled));
    }

    #[test]
    fn test_multi_line() {
        let tiled = covered(
            new_multi_line_string(vec![new_line_string(&[0.5, 0.5, 1.5, 0.5]), new_line_string(&[3.5, 1.5, 4.5, 1.5])]),
            14,
        );
        assert_eq!(HashSet::from([tc(0, 0, 14), tc(1, 0, 14), tc(3, 1, 14), tc(4, 1, 14)]), set(&tiled));
    }

    #[test]
    fn test_polygon() {
        let tiled = covered(
            new_polygon_with_holes(rectangle_coord_list(25.5, 27.5), vec![rectangle_coord_list(25.9, 27.1)]),
            14,
        );
        assert_eq!(
            HashSet::from([
                tc(25, 25, 14),
                tc(26, 25, 14),
                tc(27, 25, 14),
                tc(25, 26, 14),
                // (26, 26) skipped because of hole!
                tc(27, 26, 14),
                tc(25, 27, 14),
                tc(26, 27, 14),
                tc(27, 27, 14),
            ]),
            set(&tiled)
        );
    }

    #[test]
    fn test_multi_polygon() {
        let tiled = covered(new_multi_polygon(vec![rectangle(25.5, 26.5), rectangle(30.1, 30.9)]), 14);
        assert_eq!(
            HashSet::from([tc(25, 25, 14), tc(25, 26, 14), tc(26, 25, 14), tc(26, 26, 14), tc(30, 30, 14)]),
            set(&tiled)
        );
    }

    #[test]
    fn test_empty() {
        let tiled = covered(gc(vec![]), 14);
        assert!(set(&tiled).is_empty());
    }

    #[test]
    fn test_covered_tiles_iterator_ends() {
        let tiled = covered(new_point(0.5, 0.5), 14);
        let mut iter = tiled.iter();
        assert!(iter.next().is_some());
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_geometry_collection() {
        let tiled = covered(
            gc(vec![
                rectangle(0.1, 0.9).into(),
                new_point(1.5, 1.5).into(),
                gc(vec![new_line_string(&[3.5, 10.5, 4.5, 10.5]).into()]),
            ]),
            14,
        );
        assert_eq!(HashSet::from([tc(0, 0, 14), tc(1, 1, 14), tc(3, 10, 14), tc(4, 10, 14)]), set(&tiled));
    }

    /// JTS `AffineTransformation.rotationInstance(theta, x, y)`.
    fn rotate(seq: &mut CoordSeq, x: f64, y: f64, degrees: i32) {
        let theta = (degrees as f64).to_radians();
        let (sin, cos) = (theta.sin(), theta.cos());
        for c in seq.iter_mut() {
            let (px, py) = (c.x, c.y);
            c.x = cos * px - sin * py + (x - x * cos + y * sin);
            c.y = sin * px + cos * py + (y - x * sin - y * cos);
        }
    }

    fn flip_and_rotate(seq: &mut CoordSeq, x: f64, y: f64, flip_x: bool, flip_y: bool, degrees: i32) {
        if flip_x {
            // reflect across the vertical line through (x, y)
            for c in seq.iter_mut() {
                c.x = 2.0 * x - c.x;
            }
        }
        if flip_y {
            // reflect across the horizontal line through (x, y)
            for c in seq.iter_mut() {
                c.y = 2.0 * y - c.y;
            }
        }
        rotate(seq, x, y, degrees);
        // maintain winding order if we did a single flip, doing a second flip fixes winding order itself
        if flip_x ^ flip_y {
            seq.reverse();
        }
    }

    fn seq(coords: &[f64]) -> CoordSeq {
        let mut result = Vec::new();
        for c in coords.chunks(2) {
            add_point(&mut result, c[0], c[1]);
        }
        close_ring(&mut result);
        result
    }

    /// JTS `Orientation.isCCW` (y-up).
    fn is_ccw(ring: &[Coord<f64>]) -> bool {
        gcs::ring_signed_area(ring) < 0.0
    }

    #[test]
    fn test_only_hole_touches_other_cell_bottom() {
        for degrees in [0, 90, 180, 270] {
            // hole falls outside shell, and hole touches neighboring tile; make sure that we don't emit that other
            // tile at all
            let mut outer = seq(&[1.5, 1.5, 1.6, 1.5, 1.5, 1.6]);
            let mut inner = seq(&[1.4, 1.8, 1.6, 1.8, 1.5, 2.0]);
            rotate(&mut outer, 1.5, 1.5, degrees);
            rotate(&mut inner, 1.5, 1.5, degrees);
            let groups = vec![vec![outer, inner]];
            let extent = for_zoom(11, 0, 0, 1 << 11, 1 << 11);
            assert!(TiledGeometry::slice_into_tiles(&groups, 0.1, true, 11, &extent).is_err(), "{degrees}");
        }
    }

    const FLIP_CASES: [(i32, bool, bool); 7] = [
        (0, false, false),
        (90, false, false),
        (180, false, false),
        (270, false, false),
        (0, true, false),
        (0, false, true),
        (0, true, true),
    ];

    fn test_render(groups: Groups) -> TiledGeometry {
        // Java uses min x/y = -10, which is equivalent here since no content falls on negative tiles
        TiledGeometry::slice_into_tiles(&groups, 0.0, true, 14, &for_zoom(14, 0, 0, 1 << 14, 1 << 14)).unwrap()
    }

    #[test]
    fn test_overlapping_holes() {
        for (degrees, flip_x, flip_y) in FLIP_CASES {
            let mut outer = seq(&[1.0, 1.0, 10.0, 1.0, 10.0, 10.0, 1.0, 10.0]);
            let mut inner1 = seq(&[2.0, 2.0, 2.0, 9.0, 9.0, 9.0, 3.0, 5.0, 9.0, 2.0]);
            let mut inner2 = seq(&[9.0, 3.0, 9.0, 8.0, 4.0, 5.0]);
            flip_and_rotate(&mut outer, 6.0, 6.0, flip_x, flip_y, degrees);
            flip_and_rotate(&mut inner1, 6.0, 6.0, flip_x, flip_y, degrees);
            flip_and_rotate(&mut inner2, 6.0, 6.0, flip_x, flip_y, degrees);

            test_render(vec![vec![outer.clone(), inner1.clone()]]);
            test_render(vec![vec![outer.clone(), inner2.clone()]]);
            test_render(vec![vec![outer.clone(), inner1.clone(), inner2.clone()]]);
            let result = test_render(vec![vec![outer, inner2, inner1]]);
            if degrees == 0 && !flip_x && !flip_y {
                let covered = result.covered_tiles();
                assert!(!covered.test(7, 4));
                assert!(!covered.test(3, 3));
                assert!(covered.test(1, 1));
                assert!(covered.test(9, 9));
            }
        }
    }

    #[test]
    fn test_inside_complex_hole() {
        for (degrees, flip_x, flip_y) in FLIP_CASES {
            let mut outer = seq(&[1.0, 1.0, 10.0, 1.0, 10.0, 10.0, 1.0, 10.0]);
            let mut inner1 =
                seq(&[6.5, 1.5, 2.0, 2.0, 2.0, 9.0, 9.0, 9.0, 9.0, 2.0, 4.6, 2.0, 8.0, 8.0, 3.0, 8.0, 4.0, 2.0]);
            let mut inner2 = seq(&[5.5, 6.5, 5.5, 6.6, 5.6, 6.6]);
            flip_and_rotate(&mut outer, 6.0, 6.0, flip_x, flip_y, degrees);
            flip_and_rotate(&mut inner1, 6.0, 6.0, flip_x, flip_y, degrees);
            flip_and_rotate(&mut inner2, 6.0, 6.0, flip_x, flip_y, degrees);

            assert!(is_ccw(&outer));
            assert!(!is_ccw(&inner1));
            assert!(!is_ccw(&inner2));

            test_render(vec![vec![outer.clone(), inner1.clone()]]);
            test_render(vec![vec![outer.clone(), inner2.clone()]]);
            test_render(vec![vec![outer.clone(), inner1.clone(), inner2.clone()]]);
            let result = test_render(vec![vec![outer, inner2, inner1]]);
            if degrees == 0 && !flip_x && !flip_y {
                let filled: HashSet<(u32, u32, u32)> = result.filled_tiles().map(|t| (t.x, t.y, t.z)).collect();
                assert!(filled.contains(&tc(5, 5, 14)));
                assert!(filled.contains(&tc(4, 6, 14)));
                assert!(!filled.contains(&tc(5, 6, 14)));
            }
        }
    }

    #[test]
    fn test_side_of_hole_intercepted() {
        for (degrees, flip_x, flip_y) in FLIP_CASES {
            let mut outer = seq(&[1.0, 1.0, 10.0, 1.0, 10.0, 10.0, 1.0, 10.0]);
            let mut inner1 = seq(&[
                2.0, 2.0, 2.0, 9.0, 9.0, 9.0, 3.0, 5.0, 9.0, 2.0, 9.0, 4.2, 7.5, 4.2, 7.5, 4.8, 9.5, 4.8, 9.5, 1.8,
            ]);
            flip_and_rotate(&mut outer, 5.0, 5.0, flip_x, flip_y, degrees);
            flip_and_rotate(&mut inner1, 5.0, 5.0, flip_x, flip_y, degrees);

            let result = test_render(vec![vec![outer, inner1]]);
            if degrees == 0 && !flip_x && !flip_y {
                let filled: HashSet<(u32, u32, u32)> = result.filled_tiles().map(|t| (t.x, t.y, t.z)).collect();
                assert!(!filled.contains(&tc(7, 4, 14)), "{filled:?}");
                let tile_data = result.get_tile_data(TileCoord::of_xyz(7, 4, 14)).unwrap();
                let java_round = |v: f64| (v * 10.0 + 0.5).floor() / 10.0;
                let normalized: Groups = tile_data
                    .iter()
                    .map(|items| {
                        items
                            .iter()
                            .map(|s| s.iter().map(|c| Coord { x: java_round(c.x), y: java_round(c.y) }).collect())
                            .collect()
                    })
                    .collect();
                assert_eq!(
                    vec![vec![
                        new_coordinate_list(&[0.0, 0.0, 256.0, 0.0, 256.0, 256.0, 0.0, 256.0, 0.0, 0.0]),
                        new_coordinate_list(&[
                            -0.0, 256.0, -0.0, 0.0, 256.0, 0.0, 256.0, 51.2, 128.0, 51.2, 128.0, 204.8, 256.0, 204.8,
                            256.0, 0.0, 0.0, 0.0, 0.0, 256.0,
                        ]),
                    ]],
                    normalized
                );
            }
        }
    }

    #[test]
    fn test_encode_decode() {
        for (z, x, y) in [
            (0, 0, 0),
            (2, 0, 0),
            (2, 3, 3),
            (3, 7, 6),
            (3, 7, 7),
            (15, 0, 0),
            (15, 32767, 0),
            (15, 0, 32767),
            (15, 32767, 32767),
            (16, 0, 0),
            (16, 1, 2),
            (16, 65535, 0),
            (16, 65535, 65535),
            (16, 0, 65535),
        ] {
            let max_tiles_at_zoom = 1u64 << z;
            let encoded = encode(max_tiles_at_zoom, x, y);
            let decoded = decode(max_tiles_at_zoom, encoded as u64, z);
            assert_eq!((x as u32, y as u32, z), (decoded.x, decoded.y, decoded.z));
        }
    }

    #[test]
    fn test_wrap_around_antimeridian() {
        // a line crossing the right edge of the world at z1 shows up on the left side too
        let groups = vec![vec![new_coordinate_list(&[1.5, 0.5, 2.5, 0.5])]];
        let tiled = TiledGeometry::slice_into_tiles(&groups, 0.0, false, 1, &for_zoom(1, 0, 0, 2, 2)).unwrap();
        let tiles: HashSet<(u32, u32, u32)> = tiled.tile_data().map(|(t, _)| (t.x, t.y, t.z)).collect();
        assert_eq!(HashSet::from([tc(1, 0, 1), tc(0, 0, 1)]), tiles);
    }

    #[test]
    fn test_filled_tiles_of_large_square() {
        let groups = gcs::extract_groups(&rectangle(0.5, 4.5).into(), 0.0);
        let tiled = TiledGeometry::slice_into_tiles(&groups, 0.0, true, 3, &for_zoom(3, 0, 0, 8, 8)).unwrap();
        let filled: Vec<(u32, u32)> = tiled.filled_tiles().map(|t| (t.x, t.y)).collect();
        assert_eq!(vec![(1, 1), (1, 2), (1, 3), (2, 1), (2, 2), (2, 3), (3, 1), (3, 2), (3, 3)], filled);
        assert_eq!(16, tiled.num_tiles_with_data());
        assert_eq!(25, tiled.covered_tiles().cardinality());
    }
}
